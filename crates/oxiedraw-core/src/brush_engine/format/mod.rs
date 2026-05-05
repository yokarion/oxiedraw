mod error;
mod load;
mod save;
mod types;

pub use error::BrushError;
pub use load::load;
pub use save::save;
pub use types::{BrushDocument, BrushManifest, BrushPackage, FamilyDoc, SCHEMA_VERSION};

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

        let restored_data = match restored.family {
            BrushFamily::Textured(rc) => rc,
            _ => panic!("expected textured"),
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
}
