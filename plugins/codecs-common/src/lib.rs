//! Common raster format codecs (PNG, JPEG, WebP, TIFF) via the `image`
//! crate, wrapped as `CodecPlugin`s, plus layered Affinity
//! import/export. For the simple formats, import produces a single
//! "Background" layer and export flattens through the compositor.

pub use affinity::AffinityCodec;
use anyhow::Context as _;
use image::ImageFormat;
use schist_color::Depth;
use schist_core::{blit_rgba8, blit_rgba_f32, Document, IntRect, Layer};
use schist_plugin_api::{CodecPlugin, ExportOptions, PluginManifest, PluginRegistry};

mod affinity;

/// Which depth a decoded image deserves.
///
/// Everything used to land on `Depth::Eight`: `img.to_rgba8()` and
/// `Document::new(.., Depth::Eight)`. A 16-bit png or a 16/32-bit float
/// tiff lost half its precision or more on the way in, permanently and
/// with no warning.
fn depth_for(color: image::ColorType) -> Depth {
    use image::ColorType::*;
    match color {
        L16 | La16 | Rgb16 | Rgba16 => Depth::Sixteen,
        Rgb32F | Rgba32F => Depth::ThirtyTwo,
        _ => Depth::Eight,
    }
}

fn import_with(format: ImageFormat, bytes: &[u8], title: &str) -> anyhow::Result<Document> {
    let img = image::load_from_memory_with_format(bytes, format)
        .with_context(|| format!("decoding {title}"))?;
    let depth = depth_for(img.color());
    let (w, h) = (img.width(), img.height());
    anyhow::ensure!(w > 0 && h > 0, "zero-sized image");
    let mut doc = Document::new(title, w, h, depth);
    let mut layer = Layer::new_raster("Background");
    let tiles = &mut layer.as_raster_mut().unwrap().tiles;
    let rect = IntRect::from_size(w, h);
    match depth {
        Depth::Eight => blit_rgba8(tiles, depth, rect, img.to_rgba8().as_raw()),
        // 16-bit and float sources keep their precision: the u8 blit
        // would quantise them on the way in.
        Depth::Sixteen => {
            let src = img.to_rgba16();
            let f32s: Vec<f32> = src.as_raw().iter().map(|&v| v as f32 / 65535.0).collect();
            blit_rgba_f32(tiles, depth, rect, &f32s);
        }
        Depth::ThirtyTwo => {
            let src = img.to_rgba32f();
            blit_rgba_f32(tiles, depth, rect, src.as_raw());
        }
    }
    doc.push_layer(layer);
    doc.damage_all();
    doc.dirty = false;
    Ok(doc)
}

fn export_flat(
    doc: &Document,
    format: ImageFormat,
    options: &ExportOptions,
) -> anyhow::Result<Vec<u8>> {
    let region = doc.canvas_rect();
    let mut pixels = schist_compositor::composite_region_f32(doc, region);
    // Reducing to 8 bits per channel bands smooth gradients; dither unless
    // the user turned it off.
    if options.dither && options.bit_depth <= 8 {
        schist_colormgmt::dither_to_depth(&mut pixels, doc.width as usize, 1 << options.bit_depth);
    }
    let mut out = std::io::Cursor::new(Vec::new());
    // `bit_depth` was only ever consulted to pick the dither level, so
    // "export 16-bit png" was not achievable: every path built an
    // 8-bit `RgbaImage`. png and tiff can carry 16 bits per channel;
    // jpeg and webp cannot, so they stay at 8 whatever is asked.
    let sixteen = options.bit_depth > 8 && matches!(format, ImageFormat::Png | ImageFormat::Tiff);
    if sixteen {
        let rgba: Vec<u16> = pixels
            .iter()
            .map(|v| (v.clamp(0.0, 1.0) * 65535.0 + 0.5) as u16)
            .collect();
        let img: image::ImageBuffer<image::Rgba<u16>, Vec<u16>> =
            image::ImageBuffer::from_raw(doc.width, doc.height, rgba).context("buffer size")?;
        img.write_to(&mut out, format)?;
        return Ok(out.into_inner());
    }

    let rgba: Vec<u8> = pixels
        .iter()
        .map(|v| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)
        .collect();
    let img: image::RgbaImage =
        image::ImageBuffer::from_raw(doc.width, doc.height, rgba).context("buffer size")?;
    match format {
        // JPEG has no alpha and takes a quality setting.
        ImageFormat::Jpeg => {
            let rgb = image::DynamicImage::ImageRgba8(img).to_rgb8();
            let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(
                &mut out,
                options.quality.clamp(1, 100),
            );
            encoder.encode_image(&rgb)?;
        }
        _ => img.write_to(&mut out, format)?,
    }
    Ok(out.into_inner())
}

macro_rules! simple_codec {
    ($ty:ident, $id:literal, $name:literal, $format:expr, $exts:expr, $magic:expr) => {
        pub struct $ty;

        impl CodecPlugin for $ty {
            fn id(&self) -> &'static str {
                $id
            }
            fn name(&self) -> &'static str {
                $name
            }
            fn extensions(&self) -> &'static [&'static str] {
                $exts
            }
            fn probe(&self, bytes: &[u8]) -> bool {
                let magic: &[&[u8]] = $magic;
                magic.iter().any(|m| bytes.starts_with(m))
            }
            fn import(&self, bytes: &[u8]) -> anyhow::Result<Document> {
                import_with($format, bytes, $name)
            }
            fn can_export(&self) -> bool {
                true
            }
            fn export(&self, doc: &Document) -> anyhow::Result<Vec<u8>> {
                export_flat(doc, $format, &ExportOptions::default())
            }
            fn export_with(
                &self,
                doc: &Document,
                options: &ExportOptions,
            ) -> anyhow::Result<Vec<u8>> {
                export_flat(doc, $format, options)
            }
            fn supports_quality(&self) -> bool {
                matches!($format, ImageFormat::Jpeg)
            }
        }
    };
}

simple_codec!(
    PngCodec,
    "codec.png",
    "PNG",
    ImageFormat::Png,
    &["png"],
    &[b"\x89PNG"]
);
simple_codec!(
    JpegCodec,
    "codec.jpeg",
    "JPEG",
    ImageFormat::Jpeg,
    &["jpg", "jpeg"],
    &[b"\xFF\xD8\xFF"]
);
simple_codec!(
    WebPCodec,
    "codec.webp",
    "WebP",
    ImageFormat::WebP,
    &["webp"],
    &[b"RIFF"]
);
simple_codec!(
    TiffCodec,
    "codec.tiff",
    "TIFF",
    ImageFormat::Tiff,
    &["tif", "tiff"],
    &[b"II*\x00", b"MM\x00*"]
);

pub struct CommonCodecsPlugin;

impl PluginManifest for CommonCodecsPlugin {
    fn id(&self) -> &'static str {
        "schist.codecs-common"
    }

    fn register(&self, registry: &mut PluginRegistry) {
        registry.register_codec(Box::new(PngCodec));
        registry.register_codec(Box::new(JpegCodec));
        registry.register_codec(Box::new(WebPCodec));
        registry.register_codec(Box::new(TiffCodec));
        registry.register_codec(Box::new(AffinityCodec));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_round_trip() {
        // Encode a small test image, import as document, export, re-import.
        let mut img = image::RgbaImage::new(20, 10);
        for (x, _y, p) in img.enumerate_pixels_mut() {
            *p = image::Rgba([x as u8 * 10, 100, 200, 255]);
        }
        let mut bytes = std::io::Cursor::new(Vec::new());
        img.write_to(&mut bytes, ImageFormat::Png).unwrap();
        let bytes = bytes.into_inner();

        let codec = PngCodec;
        assert!(codec.probe(&bytes));
        let doc = codec.import(&bytes).unwrap();
        assert_eq!((doc.width, doc.height), (20, 10));
        let layer = doc.tree.layers.first().unwrap();
        let px = layer.as_raster().unwrap().tiles.pixel(3, 0).to_u8();
        assert_eq!(px, [30, 100, 200, 255]);

        let out = codec.export(&doc).unwrap();
        let doc2 = codec.import(&out).unwrap();
        let px2 = doc2.tree.layers[0]
            .as_raster()
            .unwrap()
            .tiles
            .pixel(3, 0)
            .to_u8();
        assert_eq!(px2, [30, 100, 200, 255]);
    }

    /// Every non-psd import was forced through `to_rgba8()` and
    /// `Document::new(.., Depth::Eight)`, so a 16-bit scan lost half its
    /// precision permanently and with no warning.
    #[test]
    fn a_sixteen_bit_png_keeps_its_precision() {
        let mut img: image::ImageBuffer<image::Rgba<u16>, Vec<u16>> = image::ImageBuffer::new(4, 2);
        // A value that has no 8-bit representation: 0x0101 is the nearest
        // 8-bit-expressible neighbour either side.
        for (_, _, p) in img.enumerate_pixels_mut() {
            *p = image::Rgba([0x0180, 0x8000, 0xFFFF, 0xFFFF]);
        }
        let mut bytes = std::io::Cursor::new(Vec::new());
        img.write_to(&mut bytes, ImageFormat::Png).unwrap();

        let doc = PngCodec.import(&bytes.into_inner()).unwrap();
        assert_eq!(doc.depth, Depth::Sixteen);
        let px = doc.tree.layers[0].as_raster().unwrap().tiles.pixel(1, 1);
        // 0x0180 / 65535 == 0.005889..., which rounds to 2/255 == 0.00784
        // if it goes through 8 bits.
        assert!(
            (px.r - 0x0180 as f32 / 65535.0).abs() < 1e-4,
            "red came back as {} (8-bit quantised is {})",
            px.r,
            2.0 / 255.0
        );
    }

    /// And an 8-bit source stays 8-bit, so ordinary files do not quadruple
    /// in memory for nothing.
    #[test]
    fn an_eight_bit_png_stays_eight_bit() {
        let mut img = image::RgbaImage::new(4, 2);
        img.fill(200);
        let mut bytes = std::io::Cursor::new(Vec::new());
        img.write_to(&mut bytes, ImageFormat::Png).unwrap();
        assert_eq!(
            PngCodec.import(&bytes.into_inner()).unwrap().depth,
            Depth::Eight
        );
    }

    /// `bit_depth` was only consulted to pick the dither level, never to
    /// choose an output depth, so "export 16-bit png" was unreachable.
    #[test]
    fn export_honours_the_requested_bit_depth() {
        let mut doc = Document::new("t", 8, 4, Depth::Sixteen);
        let mut layer = Layer::new_raster("Background");
        let buf: Vec<f32> = [0.00589f32, 0.5, 1.0, 1.0].repeat(8 * 4);
        schist_core::blit_rgba_f32(
            &mut layer.as_raster_mut().unwrap().tiles,
            Depth::Sixteen,
            IntRect::from_size(8, 4),
            &buf,
        );
        doc.push_layer(layer);

        let deep = PngCodec
            .export_with(
                &doc,
                &ExportOptions {
                    bit_depth: 16,
                    dither: false,
                    ..Default::default()
                },
            )
            .unwrap();
        let back = PngCodec.import(&deep).unwrap();
        assert_eq!(back.depth, Depth::Sixteen, "export dropped to 8 bits");
        let px = back.tree.layers[0].as_raster().unwrap().tiles.pixel(1, 1);
        assert!((px.r - 0.00589).abs() < 1e-4, "got {}", px.r);

        // The default is still 8-bit.
        let shallow = PngCodec.export(&doc).unwrap();
        assert_eq!(PngCodec.import(&shallow).unwrap().depth, Depth::Eight);
    }

    /// jpeg cannot carry 16 bits, so asking for it must not fail the
    /// export.
    #[test]
    fn a_format_without_sixteen_bit_still_exports() {
        let mut doc = Document::new("t", 8, 4, Depth::Eight);
        doc.push_layer(Layer::new_raster("Background"));
        let bytes = JpegCodec
            .export_with(
                &doc,
                &ExportOptions {
                    bit_depth: 16,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(JpegCodec.probe(&bytes));
    }

    #[test]
    fn registry_probe_dispatch() {
        let mut reg = PluginRegistry::new();
        CommonCodecsPlugin.register(&mut reg);
        assert_eq!(
            reg.codec_for(b"\x89PNG....", None).unwrap().id(),
            "codec.png"
        );
        assert_eq!(
            reg.codec_for(b"\xFF\xD8\xFF\xE0", None).unwrap().id(),
            "codec.jpeg"
        );
        assert_eq!(reg.codec_for(b"", Some("tiff")).unwrap().id(), "codec.tiff");
        assert_eq!(
            reg.codec_for(b"\x00\xFFKA....", None).unwrap().id(),
            "codec.affinity"
        );
        assert_eq!(
            reg.codec_for(b"", Some("afphoto")).unwrap().id(),
            "codec.affinity"
        );
        assert!(reg.codec_for(b"garbage", Some("xyz")).is_none());
    }
}
