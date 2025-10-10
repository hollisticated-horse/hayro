use crate::Id;
use crate::SvgRenderer;
use crate::mask::{ImageLuminanceMask, MaskKind};
use base64::Engine;
use hayro_interpret::{Device, FillRule, LumaData, Paint, PathDrawMode, RgbData};
use image::codecs::png::PngEncoder;
use image::{ColorType, DynamicImage, ImageBuffer, ImageEncoder, RgbImage};
use kurbo::{Affine, Rect, Shape};
use std::sync::Arc;
use std::thread_local;

thread_local! {
    static PNG_BUFFER: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(Vec::new());
    static DATA_URI: std::cell::RefCell<String> = std::cell::RefCell::new(String::new());
}

impl<'a> SvgRenderer<'a> {
    pub(crate) fn draw_rgba_image(
        &mut self,
        rgb_data: RgbData,
        transform: Affine,
        alpha: Option<LumaData>,
    ) {
        if let Some(alpha) = alpha {
            if alpha.interpolate == rgb_data.interpolate
                && alpha.width == rgb_data.width
                && alpha.height == rgb_data.height
            {
                let interleaved = rgb_data
                    .data
                    .chunks(3)
                    .zip(alpha.data)
                    .flat_map(|(rgb, a)| [rgb[0], rgb[1], rgb[2], a])
                    .collect::<Vec<u8>>();

                self.write_image_bytes(
                    rgb_data.width,
                    rgb_data.height,
                    ColorType::Rgba8,
                    &interleaved,
                    rgb_data.interpolate,
                    None,
                    transform,
                );
            } else {
                let image_buf =
                    RgbImage::from_vec(rgb_data.width, rgb_data.height, rgb_data.data.clone())
                        .unwrap();

                let alpha = {
                    let image = DynamicImage::ImageLuma8(
                        ImageBuffer::from_raw(alpha.width, alpha.height, alpha.data).unwrap(),
                    );

                    let transform = transform
                        * Affine::scale_non_uniform(
                            rgb_data.width as f64 / alpha.width as f64,
                            rgb_data.height as f64 / alpha.height as f64,
                        );

                    ImageLuminanceMask {
                        image,
                        transform,
                        interpolate: alpha.interpolate,
                    }
                };

                self.push_transparency_group_inner(1.0, Some(MaskKind::Image(Arc::new(alpha))));
                self.write_image_bytes(
                    image_buf.width(),
                    image_buf.height(),
                    ColorType::Rgb8,
                    image_buf.as_raw(),
                    rgb_data.interpolate,
                    None,
                    transform,
                );
                self.pop_transparency_group();
            }
        } else {
            let RgbData {
                data,
                width,
                height,
                interpolate,
            } = rgb_data;
            self.write_image_bytes(
                width,
                height,
                ColorType::Rgb8,
                &data,
                interpolate,
                None,
                transform,
            );
        };
    }

    pub(crate) fn draw_stencil_image(
        &mut self,
        stencil: LumaData,
        transform: Affine,
        paint: &Paint<'a>,
    ) {
        let interpolate = stencil.interpolate;

        match &paint {
            Paint::Color(c) => {
                let color = c.to_rgba().to_rgba8();
                let image = stencil
                    .data
                    .iter()
                    .flat_map(|d| if *d == 255 { color } else { [0, 0, 0, 0] })
                    .collect::<Vec<u8>>();

                self.write_image_bytes(
                    stencil.width,
                    stencil.height,
                    ColorType::Rgba8,
                    &image,
                    interpolate,
                    None,
                    transform,
                );
            }
            Paint::Pattern(_) => {
                let mask = {
                    let image = DynamicImage::ImageLuma8(
                        ImageBuffer::from_raw(stencil.width, stencil.height, stencil.data).unwrap(),
                    );

                    ImageLuminanceMask {
                        image,
                        transform,
                        interpolate,
                    }
                };

                self.push_transparency_group_inner(1.0, Some(MaskKind::Image(Arc::new(mask))));
                self.draw_path(
                    &Rect::new(0.0, 0.0, stencil.width as f64, stencil.height as f64).to_path(0.1),
                    transform,
                    paint,
                    &PathDrawMode::Fill(FillRule::NonZero),
                );
                self.pop_transparency_group();
            }
        };
    }

    pub(crate) fn write_image(
        &mut self,
        image: &DynamicImage,
        interpolate: bool,
        id: Option<Id>,
        transform: Affine,
    ) {
        match image {
            DynamicImage::ImageRgb8(buf) => {
                self.write_image_bytes(
                    buf.width(),
                    buf.height(),
                    ColorType::Rgb8,
                    buf.as_raw(),
                    interpolate,
                    id,
                    transform,
                );
            }
            DynamicImage::ImageRgba8(buf) => {
                self.write_image_bytes(
                    buf.width(),
                    buf.height(),
                    ColorType::Rgba8,
                    buf.as_raw(),
                    interpolate,
                    id,
                    transform,
                );
            }
            DynamicImage::ImageLuma8(buf) => {
                self.write_image_bytes(
                    buf.width(),
                    buf.height(),
                    ColorType::L8,
                    buf.as_raw(),
                    interpolate,
                    id,
                    transform,
                );
            }
            other => {
                let rgba = other.to_rgba8();
                self.write_image_bytes(
                    rgba.width(),
                    rgba.height(),
                    ColorType::Rgba8,
                    rgba.as_raw(),
                    interpolate,
                    id,
                    transform,
                );
            }
        }
    }

    pub(crate) fn write_image_bytes(
        &mut self,
        width: u32,
        height: u32,
        color: ColorType,
        data: &[u8],
        interpolate: bool,
        id: Option<Id>,
        transform: Affine,
    ) {
        let scaling = if interpolate { "smooth" } else { "pixelated" };
        let base64 = to_base64_bytes(width, height, color, data);
        self.xml.start_element("image");
        if let Some(id) = id {
            self.xml.write_attribute("id", &id);
        }
        self.write_transform(transform);
        self.xml.write_attribute("xlink:href", &base64);
        self.xml.write_attribute("width", &width);
        self.xml.write_attribute("height", &height);
        self.xml.write_attribute("preserveAspectRatio", "none");
        self.xml
            .write_attribute("style", &format_args!("image-rendering: {scaling}"));
        self.xml.end_element();
    }
}

fn to_base64_bytes(width: u32, height: u32, color: ColorType, data: &[u8]) -> String {
    PNG_BUFFER.with(|png| {
        let mut png = png.borrow_mut();
        png.clear();
        {
            let encoder = PngEncoder::new(&mut *png);
            encoder
                .write_image(data, width, height, color.into())
                .expect("PNG encoding failed");
        }

        DATA_URI.with(|uri| {
            let mut uri = uri.borrow_mut();
            uri.clear();
            uri.push_str("data:image/png;base64,");
            base64::engine::general_purpose::STANDARD.encode_string(&*png, &mut uri);
            uri.clone()
        })
    })
}
