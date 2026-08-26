//! Common raster format codecs (PNG, JPEG, WebP, TIFF) via the `image`
//! crate, wrapped as `CodecPlugin`s, plus layered Affinity
//! import/export. For the simple formats, import produces a single
//! "Background" layer and export flattens through the compositor.

pub use affinity::AffinityCodec;
use anyhow::Context as _;
use image::{ExtendedColorType, ImageDecoder, ImageEncoder, ImageFormat};
use schist_color::Depth;
use schist_core::{blit_rgba8, Document, IntRect, Layer};
use schist_plugin_api::{CodecPlugin, ExportOptions, PluginManifest, PluginRegistry};

mod affinity;

fn import_with(format: ImageFormat, bytes: &[u8], title: &str) -> anyhow::Result<Document> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes));
    reader.set_format(format);
    let mut decoder = reader
        .into_decoder()
        .with_context(|| format!("decoding {title}"))?;
    // The embedded profile is what says which colours the numbers in the
    // file mean. Dropping it made a Display P3 png open as sRGB and show
    // over-saturated, and round-tripping one through Schist destroyed its
    // colour silently.
    let icc = decoder
        .icc_profile()
        .ok()
        .flatten()
        .filter(|p| !p.is_empty());
    let img =
        image::DynamicImage::from_decoder(decoder).with_context(|| format!("decoding {title}"))?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    anyhow::ensure!(w > 0 && h > 0, "zero-sized image");
    let mut doc = Document::new(title, w, h, Depth::Eight);
    doc.icc_profile = icc;
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
    // Carry the document's profile into the file, so everyone else sees
    // the colours the document was edited in.
    let icc = doc.icc_profile.clone().filter(|p| !p.is_empty());
    let (w, h) = (doc.width, doc.height);
    match format {
        // JPEG has no alpha and takes a quality setting.
        ImageFormat::Jpeg => {
            let rgb = image::DynamicImage::ImageRgba8(img).to_rgb8();
            let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(
                &mut out,
                options.quality.clamp(1, 100),
            );
            set_icc(&mut encoder, icc, "JPEG");
            encoder.write_image(&rgb, w, h, ExtendedColorType::Rgb8)?;
        }
        ImageFormat::Png => {
            let mut encoder = image::codecs::png::PngEncoder::new(&mut out);
            set_icc(&mut encoder, icc, "PNG");
            encoder.write_image(&img, w, h, ExtendedColorType::Rgba8)?;
        }
        ImageFormat::WebP => {
            let mut encoder = image::codecs::webp::WebPEncoder::new_lossless(&mut out);
            set_icc(&mut encoder, icc, "WebP");
            encoder.write_image(&img, w, h, ExtendedColorType::Rgba8)?;
        }
        ImageFormat::Tiff => {
            let mut encoder = image::codecs::tiff::TiffEncoder::new(&mut out);
            set_icc(&mut encoder, icc, "TIFF");
            encoder.write_image(&img, w, h, ExtendedColorType::Rgba8)?;
        }
        _ => img.write_to(&mut out, format)?,
    }
    Ok(out.into_inner())
}

/// Attach a profile, or say so in the log if this encoder cannot take one.
///
/// A profile the file cannot carry is worth a line in the log rather than
/// a failed export: the pixels are still right, they are just untagged.
fn set_icc(encoder: &mut impl ImageEncoder, icc: Option<Vec<u8>>, format: &str) {
    let Some(icc) = icc else { return };
    if let Err(err) = encoder.set_icc_profile(icc) {
        log::warn!("{format} cannot embed an ICC profile: {err}");
    }
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

    /// A minimal but structurally valid ICC v4 profile: a 132-byte
    /// header with the size and signature the decoders check, and an
    /// empty tag table.
    fn fake_icc() -> Vec<u8> {
        let mut p = vec![0u8; 132];
        p[0..4].copy_from_slice(&132u32.to_be_bytes());
        p[36..40].copy_from_slice(b"acsp");
        p
    }

    #[test]
    fn icc_profiles_survive_the_round_trip() {
        // The profile is what says which colours the numbers in the file
        // mean. Neither import nor export touched it, so a Display P3 png
        // opened as sRGB and showed over-saturated, and exporting a
        // wide-gamut document dropped the tag for everyone downstream.
        let icc = fake_icc();
        for (codec, name) in [
            (&PngCodec as &dyn CodecPlugin, "PNG"),
            (&WebPCodec as &dyn CodecPlugin, "WebP"),
            (&JpegCodec as &dyn CodecPlugin, "JPEG"),
        ] {
            let mut doc = Document::new("t", 8, 4, Depth::Eight);
            doc.push_layer(Layer::new_raster("Background"));
            doc.icc_profile = Some(icc.clone());

            let bytes = codec.export(&doc).unwrap();
            let back = codec.import(&bytes).unwrap();
            assert_eq!(
                back.icc_profile.as_deref(),
                Some(icc.as_slice()),
                "{name} lost the profile"
            );
        }
    }

    #[test]
    fn tiff_export_embeds_the_profile() {
        // TIFF export writes the IccProfile tag, but the `tiff` crate's
        // decoder does not hand it back through `icc_profile()`, so the
        // round trip cannot be asserted the way the others can. Check the
        // bytes reach the file; a TIFF reader that reads the tag will
        // find them.
        let icc = fake_icc();
        let mut doc = Document::new("t", 8, 4, Depth::Eight);
        doc.push_layer(Layer::new_raster("Background"));
        doc.icc_profile = Some(icc.clone());
        let bytes = TiffCodec.export(&doc).unwrap();
        assert!(bytes.windows(icc.len()).any(|w| w == icc.as_slice()));
    }

    #[test]
    fn an_untagged_file_stays_untagged() {
        let mut img = image::RgbaImage::new(4, 4);
        img.fill(255);
        let mut bytes = std::io::Cursor::new(Vec::new());
        img.write_to(&mut bytes, ImageFormat::Png).unwrap();
        let doc = PngCodec.import(&bytes.into_inner()).unwrap();
        assert!(doc.icc_profile.is_none());
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
