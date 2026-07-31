mod error;
mod load;
mod save;
mod types;

pub use error::BrushError;
pub use load::load;
pub use save::save;
pub use types::{
    BUILTIN_REVISION, BrushDocument, BrushManifest, BrushPackage, FamilyDoc, SCHEMA_VERSION,
};

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use super::super::brush::BrushPresetId;
    use super::super::dynamics::{Curve, DynSource, Dynamics, Mapping};
    use super::super::pattern::PatternData;
    use super::super::preset::{BrushFamily, BrushPreset};

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "oxiedraw-brush-test-{}-{name}.oxiebrush",
            std::process::id()
        ));
        p
    }

    #[test]
    fn round_trip_pixel_with_dynamics() {
        let preset = BrushPreset {
            id: BrushPresetId(42),
            name: "Trip Pixel".into(),
            family: BrushFamily::Pixel,
            default_size: 3.0,
            default_opacity: 0.8,
            spacing_ratio: 0.4,
            stabilizer: 0.0,
            speed_smoothing: 0.0,
            buildup: false,
            hardness: 1.0,
            tip: super::super::preset::TipShape::Round,
            texture_scale: 0.0,
            texture_strength: 0.0,
            texturing_mode: super::super::preset::TexturingMode::Multiply,
            dynamics: Dynamics {
                size: Some(Mapping {
                    source: DynSource::Speed,
                    curve: Curve::linear(),
                    range: (0.2, 1.0),
                    invert: true,
                }),
                ..Dynamics::default()
            },
            icon: Some(vec![0xDE, 0xAD, 0xBE, 0xEF]),
            preview: None,
            source_path: None,
        };
        let path = temp_path("pixel");
        super::save::save(&preset, &path).expect("save");
        let pkg = super::load::load(&path).expect("load");
        let restored = pkg.into_preset(BrushPresetId(99), None).expect("into_preset");
        let _ = std::fs::remove_file(&path);

        assert_eq!(restored.id, BrushPresetId(99));
        assert_eq!(restored.name, "Trip Pixel");
        assert!(matches!(restored.family, BrushFamily::Pixel));
        assert!((restored.default_size - 3.0).abs() < 1e-6);
        assert!((restored.spacing_ratio - 0.4).abs() < 1e-6);
        let size_mapping = restored.dynamics.size.as_ref().expect("size mapping");
        assert_eq!(size_mapping.source, DynSource::Speed);
        assert!(size_mapping.invert);
        assert!((size_mapping.range.0 - 0.2).abs() < 1e-6);
        assert_eq!(restored.icon.as_deref(), Some([0xDE, 0xAD, 0xBE, 0xEF].as_slice()));
    }

    #[test]
    fn round_trip_textured_preserves_pattern_alpha() {
        // 4x4 pattern with corner alpha - covers the premul -> straight
        // -> premul round-trip through PNG.
        let mut rgba = vec![0u8; 4 * 4 * 4];
        for y in 0..4 {
            for x in 0..4 {
                let i = (y * 4 + x) * 4;
                let a = ((x + y) * 30).min(255) as u8;
                rgba[i] = a;
                rgba[i + 1] = a;
                rgba[i + 2] = a;
                rgba[i + 3] = a;
            }
        }
        let preset = BrushPreset {
            id: BrushPresetId(0),
            name: "Trip Tex".into(),
            family: BrushFamily::Textured(Rc::new(PatternData::new(rgba.clone(), 4, 4))),
            default_size: 14.0,
            default_opacity: 1.0,
            spacing_ratio: 0.1,
            stabilizer: 0.0,
            speed_smoothing: 0.0,
            buildup: false,
            hardness: 1.0,
            tip: super::super::preset::TipShape::Round,
            texture_scale: 0.0,
            texture_strength: 0.0,
            texturing_mode: super::super::preset::TexturingMode::Multiply,
            dynamics: Dynamics::default(),
            icon: None,
            preview: None,
            source_path: None,
        };
        let path = temp_path("textured");
        super::save::save(&preset, &path).expect("save");
        let pkg = super::load::load(&path).expect("load");
        let restored = pkg.into_preset(BrushPresetId(7), None).expect("into_preset");
        let _ = std::fs::remove_file(&path);

        let BrushFamily::Textured(restored_data) = restored.family else {
            panic!("expected textured")
        };
        assert_eq!(restored_data.width, 4);
        assert_eq!(restored_data.height, 4);
        // PNG re-encode of premul -> straight -> premul is lossy by +/-1
        // for non-trivial alpha; tolerate that.
        for (orig, got) in rgba.iter().zip(restored_data.rgba.iter()) {
            assert!(
                orig.abs_diff(*got) <= 2,
                "byte differs by more than 2: orig={orig} got={got}"
            );
        }
    }

    #[test]
    fn round_trip_image_tip_preserves_tip_grain_and_mode() {
        let mk = |v: u8| {
            let mut rgba = vec![0u8; 4 * 4 * 4];
            for px in rgba.chunks_exact_mut(4) {
                px.copy_from_slice(&[v, v, v, v]);
            }
            Rc::new(PatternData::new(rgba, 4, 4))
        };
        let preset = BrushPreset {
            id: BrushPresetId(0),
            name: "Trip ImageTip".into(),
            family: BrushFamily::ImageTip {
                tip: mk(200),
                grain: Some(mk(120)),
            },
            default_size: 30.0,
            default_opacity: 1.0,
            spacing_ratio: 0.06,
            stabilizer: 0.0,
            speed_smoothing: 0.0,
            buildup: false,
            hardness: 1.0,
            tip: super::super::preset::TipShape::Round,
            texture_scale: 512.0,
            texture_strength: 1.0,
            texturing_mode: super::super::preset::TexturingMode::Subtract,
            dynamics: Dynamics::default(),
            icon: None,
            preview: None,
            source_path: None,
        };
        let path = temp_path("image_tip");
        super::save::save(&preset, &path).expect("save");
        let pkg = super::load::load(&path).expect("load");
        let restored = pkg.into_preset(BrushPresetId(9), None).expect("into_preset");
        let _ = std::fs::remove_file(&path);

        assert_eq!(restored.texturing_mode, super::super::preset::TexturingMode::Subtract);
        let BrushFamily::ImageTip { tip, grain } = restored.family else {
            panic!("expected image tip")
        };
        assert_eq!(tip.width, 4);
        let grain = grain.expect("grain preserved");
        assert!(tip.rgba[3].abs_diff(200) <= 2, "tip alpha preserved");
        assert!(grain.rgba[3].abs_diff(120) <= 2, "grain alpha preserved");
    }

    #[test]
    fn round_trip_smudge_family_and_dynamics() {
        // The real Real Brush preset - exercises the Smudge family + the
        // smudge dynamics serialising through the archive.
        let preset = BrushPreset::real_brush(BrushPresetId(0));
        assert!(matches!(preset.family, BrushFamily::Smudge));
        let path = temp_path("smudge");
        super::save::save(&preset, &path).expect("save");
        let pkg = super::load::load(&path).expect("load");
        let restored = pkg.into_preset(BrushPresetId(3), None).expect("into_preset");
        let _ = std::fs::remove_file(&path);

        assert!(matches!(restored.family, BrushFamily::Smudge));
        // Real Brush drives colour rate + size by pressure; smudge rate is left
        // constant (its dynamic was removed to stop the deposit pulsing).
        assert!(restored.dynamics.color_rate.is_some());
        assert!(restored.dynamics.size.is_some());
    }

    #[test]
    fn round_trip_charcoal_pencil_texture_and_dynamics() {
        // Charcoal Pencil is a Textured brush carrying the real dotted-paper
        // grain baked from a bundled PNG - exercises the full-size texture
        // round-tripping plus its pressure->size/flow/scatter dynamics.
        let preset = BrushPreset::charcoal_pencil(BrushPresetId(0));
        let path = temp_path("charcoal");
        super::save::save(&preset, &path).expect("save");
        let pkg = super::load::load(&path).expect("load");
        let restored = pkg.into_preset(BrushPresetId(7), None).expect("into_preset");
        let _ = std::fs::remove_file(&path);

        assert_eq!(
            restored.texturing_mode,
            super::super::preset::TexturingMode::Multiply
        );
        let BrushFamily::Textured(grain) = restored.family else {
            panic!("expected textured")
        };
        assert_eq!(grain.width, 512);
        assert_eq!(grain.height, 512);
        assert!(restored.dynamics.size.is_some());
        assert!(restored.dynamics.flow.is_some());
        assert!(restored.dynamics.scatter.is_some());
    }
}
