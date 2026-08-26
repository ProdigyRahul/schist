//! Common raster format codecs (PNG, JPEG, WebP, TIFF) via the `image`
//! crate, wrapped as `CodecPlugin`s, plus layered Affinity
//! import/export. For the simple formats, import produces a single
//! "Background" layer and export flattens through the compositor.

pub use affinity::AffinityCodec;
use anyhow::Context as _;
use image::ImageFormat;
use schist_color::Depth;
use schist_core::{blit_rgba8, Document, IntRect, Layer};
use schist_plugin_api::{CodecPlugin, ExportOptions, PluginManifest, PluginRegistry};

mod affinity;

fn import_with(format: ImageFormat, bytes: &[u8], title: &str) -> anyhow::Result<Document> {
    let img = image::load_from_memory_with_format(bytes, format)
        .with_context(|| format!("decoding {title}"))?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    anyhow::ensure!(w > 0 && h > 0, "zero-sized image");
    let mut doc = Document::new(title, w, h, Depth::Eight);
    let mut layer = Layer::new_raster("Background");
    blit_rgba8(
        &mut layer.as_raster_mut().unwrap().tiles,
        Depth::Eight,
        IntRect::from_size(w, h),
        rgba.as_raw(),
    );
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
    let rgba: Vec<u8> = pixels
        .iter()
        .map(|v| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)
        .collect();
    let img: image::RgbaImage =
        image::ImageBuffer::from_raw(doc.width, doc.height, rgba).context("buffer size")?;
    let mut out = std::io::Cursor::new(Vec::new());
    match format {
        // JPEG has no alpha and takes a quality setting.
        ImageFormat::Jpeg => {
            // JPEG has no alpha, and `to_rgb8` simply drops it. A straight
            // alpha composite leaves rgb at 0 where nothing was painted,
            // so every transparent area came out black. Matte onto white
            // instead, which is what Photoshop offers by default.
            let mut rgb = image::RgbImage::new(doc.width, doc.height);
            for (dst, src) in rgb.pixels_mut().zip(img.pixels()) {
                let a = src[3] as f32 / 255.0;
                let matte = |c: u8| (c as f32 * a + 255.0 * (1.0 - a)).round() as u8;
                *dst = image::Rgb([matte(src[0]), matte(src[1]), matte(src[2])]);
            }
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
                // Each alternative is a list of (offset, bytes) that must
                // all match. A bare prefix is not enough for every format:
                // "RIFF" alone matches wav and avi as well as webp.
                let magic: &[&[(usize, &[u8])]] = $magic;
                magic.iter().any(|alt| {
                    alt.iter().all(|(at, want)| {
                        bytes
                            .get(*at..at + want.len())
                            .is_some_and(|got| got == *want)
                    })
                })
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
    &[&[(0, b"\x89PNG")]]
);
simple_codec!(
    JpegCodec,
    "codec.jpeg",
    "JPEG",
    ImageFormat::Jpeg,
    &["jpg", "jpeg"],
    &[&[(0, b"\xFF\xD8\xFF")]]
);
simple_codec!(
    WebPCodec,
    "codec.webp",
    "WebP",
    ImageFormat::WebP,
    &["webp"],
    // "RIFF" alone is any RIFF container; webp also declares itself at
    // offset 8. Without that, a .wav was handed to the webp decoder,
    // because `decode_file` probes before it looks at the extension.
    &[&[(0, b"RIFF"), (8, b"WEBP")]]
);
simple_codec!(
    TiffCodec,
    "codec.tiff",
    "TIFF",
    ImageFormat::Tiff,
    &["tif", "tiff"],
    // Classic TIFF plus BigTIFF, which uses version 43 instead of 42.
    &[
        &[(0, b"II*\x00")],
        &[(0, b"MM\x00*")],
        &[(0, b"II+\x00")],
        &[(0, b"MM\x00+")],
    ]
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
    #[test]
    fn the_webp_probe_does_not_claim_every_riff_file() {
        // `decode_file` probes before it consults the extension, so a wav
        // was handed to the webp decoder and failed with "decoding WebP".
        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&[0; 4]);
        wav.extend_from_slice(b"WAVEfmt ");
        assert!(!WebPCodec.probe(&wav), "a wav is not a webp");

        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&[0; 4]);
        webp.extend_from_slice(b"WEBPVP8 ");
        assert!(WebPCodec.probe(&webp), "a real webp must still probe");
    }

    #[test]
    fn the_tiff_probe_accepts_bigtiff() {
        assert!(TiffCodec.probe(b"II*\x00rest"), "classic little-endian");
        assert!(TiffCodec.probe(b"MM\x00*rest"), "classic big-endian");
        assert!(TiffCodec.probe(b"II+\x00rest"), "bigtiff little-endian");
        assert!(TiffCodec.probe(b"MM\x00+rest"), "bigtiff big-endian");
        assert!(!TiffCodec.probe(b"II!\x00rest"), "and nothing else");
    }

    #[test]
    fn transparent_areas_export_to_jpeg_as_white_not_black() {
        // JPEG has no alpha and `to_rgb8` just drops it. A straight-alpha
        // composite leaves rgb at 0 where nothing was painted, so every
        // transparent region came out black.
        let mut doc = Document::new("t", 8, 8, Depth::Eight);
        doc.push_layer(Layer::new_raster("empty"));
        let bytes = export_flat(&doc, ImageFormat::Jpeg, &ExportOptions::default()).unwrap();

        let img = image::load_from_memory_with_format(&bytes, ImageFormat::Jpeg)
            .unwrap()
            .to_rgb8();
        let px = img.get_pixel(4, 4);
        assert!(
            px[0] > 200 && px[1] > 200 && px[2] > 200,
            "transparent should matte to white, got {px:?}"
        );
    }
}
