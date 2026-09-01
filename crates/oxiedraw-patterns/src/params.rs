//! Pattern parameters: the schema a generator publishes, the values a document
//! stores, and the input modulation bound to them.
//!
//! A schema is a `&'static [ParamDef]`, which is everything the UI needs to
//! build its panel. Stored [`Params`] are deliberately forgiving - unknown ids
//! ignored, missing ones falling back to the default - so a generator can gain,
//! drop or re-range a knob without invalidating older documents.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ParamKind {
    Float {
        min: f32,
        max: f32,
        default: f32,
        step: f32,
    },
    Int {
        min: i32,
        max: i32,
        default: i32,
    },
    Bool {
        default: bool,
    },
    Choice {
        options: &'static [&'static str],
        default: u32,
    },
    /// Stored as two values, under `<id>.lo` and `<id>.hi`.
    FloatRange {
        min: f32,
        max: f32,
        default: (f32, f32),
        step: f32,
    },
    IntRange {
        min: i32,
        max: i32,
        default: (i32, i32),
    },
}

impl ParamKind {
    /// Empty for the kinds that hold a single value.
    #[must_use]
    pub const fn range_parts(self) -> &'static [&'static str] {
        match self {
            Self::FloatRange { .. } | Self::IntRange { .. } => &["lo", "hi"],
            _ => &[],
        }
    }
}

/// `group` sorts controls into panel sections; `modulatable` decides whether
/// the UI offers an input binding next to the control.
#[derive(Debug, Clone, Copy)]
pub struct ParamDef {
    pub id: &'static str,
    pub label: &'static str,
    pub group: &'static str,
    pub tooltip: &'static str,
    pub kind: ParamKind,
    pub modulatable: bool,
    /// Whether the tool's panel offers this knob. A schema is also the
    /// generator's internals, most of which shape a look arrived at by eye;
    /// unexposed knobs still load, save and modulate.
    pub exposed: bool,
}

impl ParamDef {
    /// Every id this definition stores under, with its default. A range stores
    /// its two ends separately, so one that gains or loses an end still loads.
    #[must_use]
    pub fn defaults(&self) -> Vec<(String, ParamValue)> {
        match self.kind {
            ParamKind::Float { default, .. } => {
                vec![(self.id.to_owned(), ParamValue::Float(default))]
            }
            ParamKind::Int { default, .. } => {
                vec![(self.id.to_owned(), ParamValue::Int(default))]
            }
            ParamKind::Bool { default } => vec![(self.id.to_owned(), ParamValue::Bool(default))],
            ParamKind::Choice { default, .. } => {
                vec![(self.id.to_owned(), ParamValue::Choice(default))]
            }
            ParamKind::FloatRange { default, .. } => vec![
                (format!("{}.lo", self.id), ParamValue::Float(default.0)),
                (format!("{}.hi", self.id), ParamValue::Float(default.1)),
            ],
            ParamKind::IntRange { default, .. } => vec![
                (format!("{}.lo", self.id), ParamValue::Int(default.0)),
                (format!("{}.hi", self.id), ParamValue::Int(default.1)),
            ],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamValue {
    Float(f32),
    Int(i32),
    Bool(bool),
    Choice(u32),
}

/// The knob values a document holds for one pattern stroke. Only values that
/// differ from (or were explicitly set against) the schema need be present.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Params {
    #[serde(flatten)]
    values: BTreeMap<String, ParamValue>,
}

impl Params {
    /// Every schema default made explicit, so a new stroke's panel and document
    /// agree from the start.
    #[must_use]
    pub fn from_schema(schema: &[ParamDef]) -> Self {
        Self {
            values: schema.iter().flat_map(ParamDef::defaults).collect(),
        }
    }

    pub fn get(&self, id: &str) -> Option<ParamValue> {
        self.values.get(id).copied()
    }

    pub fn set(&mut self, id: &str, value: ParamValue) {
        self.values.insert(id.to_owned(), value);
    }

    pub fn set_float(&mut self, id: &str, value: f32) {
        self.set(id, ParamValue::Float(value));
    }

    pub fn set_int(&mut self, id: &str, value: i32) {
        self.set(id, ParamValue::Int(value));
    }

    pub fn set_bool(&mut self, id: &str, value: bool) {
        self.set(id, ParamValue::Bool(value));
    }

    pub fn set_choice(&mut self, id: &str, value: u32) {
        self.set(id, ParamValue::Choice(value));
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Optional housekeeping: reading tolerates strays, this keeps files tidy.
    pub fn prune(&mut self, schema: &[ParamDef]) {
        self.values.retain(|id, _| {
            schema.iter().any(|def| {
                def.id == id
                    || def
                        .kind
                        .range_parts()
                        .iter()
                        .any(|part| id.strip_prefix(def.id) == Some(&format!(".{part}")))
            })
        });
    }
}

/// Stored values resolved against a schema. Every getter answers - falling back
/// to the default when a value is absent or the wrong type, and clamping to the
/// declared range - so a generator never has to defend itself.
#[derive(Debug, Clone, Copy)]
pub struct Resolved<'a> {
    schema: &'static [ParamDef],
    values: &'a Params,
}

impl<'a> Resolved<'a> {
    pub const fn new(schema: &'static [ParamDef], values: &'a Params) -> Self {
        Self { schema, values }
    }

    pub const fn schema(&self) -> &'static [ParamDef] {
        self.schema
    }

    fn def(&self, id: &str) -> Option<&'static ParamDef> {
        self.schema.iter().find(|def| def.id == id)
    }

    pub fn float(&self, id: &str) -> f32 {
        let Some(def) = self.def(id) else {
            tracing::warn!(id, "pattern read a float not in its schema");
            return 0.0;
        };
        let ParamKind::Float {
            min, max, default, ..
        } = def.kind
        else {
            tracing::warn!(id, "pattern read {id} as a float, schema says otherwise");
            return 0.0;
        };
        match self.values.get(id) {
            Some(ParamValue::Float(v)) if v.is_finite() => v.clamp(min, max),
            Some(ParamValue::Int(v)) => (v as f32).clamp(min, max),
            _ => default,
        }
    }

    pub fn int(&self, id: &str) -> i32 {
        let Some(def) = self.def(id) else {
            tracing::warn!(id, "pattern read an int not in its schema");
            return 0;
        };
        let ParamKind::Int { min, max, default } = def.kind else {
            tracing::warn!(id, "pattern read {id} as an int, schema says otherwise");
            return 0;
        };
        match self.values.get(id) {
            Some(ParamValue::Int(v)) => v.clamp(min, max),
            Some(ParamValue::Float(v)) if v.is_finite() => (v.round() as i32).clamp(min, max),
            _ => default,
        }
    }

    /// An int knob as a non-negative count, for loop bounds.
    pub fn count(&self, id: &str) -> u32 {
        self.int(id).max(0) as u32
    }

    pub fn bool(&self, id: &str) -> bool {
        let Some(def) = self.def(id) else {
            tracing::warn!(id, "pattern read a bool not in its schema");
            return false;
        };
        let ParamKind::Bool { default } = def.kind else {
            tracing::warn!(id, "pattern read {id} as a bool, schema says otherwise");
            return false;
        };
        match self.values.get(id) {
            Some(ParamValue::Bool(v)) => v,
            _ => default,
        }
    }

    pub fn choice(&self, id: &str) -> u32 {
        let Some(def) = self.def(id) else {
            tracing::warn!(id, "pattern read a choice not in its schema");
            return 0;
        };
        let ParamKind::Choice { options, default } = def.kind else {
            tracing::warn!(id, "pattern read {id} as a choice, schema says otherwise");
            return 0;
        };
        let limit = (options.len().max(1) - 1) as u32;
        match self.values.get(id) {
            Some(ParamValue::Choice(v)) => v.min(limit),
            Some(ParamValue::Int(v)) => v.max(0).unsigned_abs().min(limit),
            _ => default.min(limit),
        }
    }

    /// Ordered, so a generator never has to check which way round they came.
    pub fn float_range(&self, id: &str) -> (f32, f32) {
        let Some(def) = self.def(id) else {
            tracing::warn!(id, "pattern read a range not in its schema");
            return (0.0, 0.0);
        };
        let ParamKind::FloatRange {
            min, max, default, ..
        } = def.kind
        else {
            tracing::warn!(id, "pattern read {id} as a range, schema says otherwise");
            return (0.0, 0.0);
        };
        let end = |suffix: &str, fallback: f32| match self.values.get(&format!("{id}.{suffix}")) {
            Some(ParamValue::Float(v)) if v.is_finite() => v.clamp(min, max),
            Some(ParamValue::Int(v)) => (v as f32).clamp(min, max),
            _ => fallback,
        };
        let (lo, hi) = (end("lo", default.0), end("hi", default.1));
        (lo.min(hi), lo.max(hi))
    }

    pub fn int_range(&self, id: &str) -> (i32, i32) {
        let Some(def) = self.def(id) else {
            tracing::warn!(id, "pattern read a range not in its schema");
            return (0, 0);
        };
        let ParamKind::IntRange { min, max, default } = def.kind else {
            tracing::warn!(id, "pattern read {id} as a range, schema says otherwise");
            return (0, 0);
        };
        let end = |suffix: &str, fallback: i32| match self.values.get(&format!("{id}.{suffix}")) {
            Some(ParamValue::Int(v)) => v.clamp(min, max),
            #[allow(clippy::cast_possible_truncation)]
            Some(ParamValue::Float(v)) if v.is_finite() => (v.round() as i32).clamp(min, max),
            _ => fallback,
        };
        let (lo, hi) = (end("lo", default.0), end("hi", default.1));
        (lo.min(hi), lo.max(hi))
    }

    pub fn modulatable(&self, id: &str) -> bool {
        self.def(id).is_some_and(|def| def.modulatable)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModSource {
    /// No modulation: the knob's value is used as-is.
    #[default]
    Constant,
    /// Stylus pressure sampled at the element's position on the curve.
    Pressure,
    /// How sharply the curve turns there (a 60 px radius reads as 1.0).
    Curvature,
    /// Smooth noise along the curve, for variation with no input to drive it.
    Noise,
    /// 0 at both ends of the curve, 1 in the middle - tapers a stroke off.
    Ends,
}

impl ModSource {
    pub const ALL: &'static [Self] = &[
        Self::Constant,
        Self::Pressure,
        Self::Curvature,
        Self::Noise,
        Self::Ends,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Constant => "None",
            Self::Pressure => "Pressure",
            Self::Curvature => "Curvature",
            Self::Noise => "Noise",
            Self::Ends => "Ends",
        }
    }
}

/// Response shape applied to a modulation source before it scales a parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Curve {
    #[default]
    Linear,
    /// Slow at first, so only firm pressure counts.
    EaseIn,
    /// Fast at first, so a light touch already reads.
    EaseOut,
    Smooth,
}

impl Curve {
    pub const ALL: &'static [Self] = &[Self::Linear, Self::EaseIn, Self::EaseOut, Self::Smooth];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Linear => "Linear",
            Self::EaseIn => "Ease in",
            Self::EaseOut => "Ease out",
            Self::Smooth => "Smooth",
        }
    }

    #[must_use]
    pub fn shape(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Self::Linear => t,
            Self::EaseIn => t * t,
            Self::EaseOut => t * (2.0 - t),
            Self::Smooth => crate::noise::smoothstep(t),
        }
    }
}

/// `amount` is how much of the parameter the source takes over, `0..=1`. At
/// `1.0` the knob sets the maximum - full pressure gives the knob's value and
/// no pressure gives zero, as the brush engine's size dynamics already behave.
/// Negative amounts invert the source.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Modulation {
    pub source: ModSource,
    pub amount: f32,
    pub curve: Curve,
}

impl Modulation {
    #[must_use]
    pub fn new(source: ModSource, amount: f32) -> Self {
        Self {
            source,
            amount,
            curve: Curve::Linear,
        }
    }

    #[must_use]
    pub fn factor(&self, source_value: f32) -> f32 {
        if self.source == ModSource::Constant || self.amount == 0.0 {
            return 1.0;
        }
        let shaped = self.curve.shape(source_value);
        let (target, amount) = if self.amount >= 0.0 {
            (shaped, self.amount)
        } else {
            (1.0 - shaped, -self.amount)
        };
        let amount = amount.clamp(0.0, 1.0);
        1.0 + (target - 1.0) * amount
    }
}

/// Per-parameter input bindings for one stroke. Empty means no modulation.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Bindings {
    #[serde(flatten)]
    map: BTreeMap<String, Modulation>,
}

impl Bindings {
    pub fn get(&self, id: &str) -> Option<Modulation> {
        self.map.get(id).copied()
    }

    pub fn set(&mut self, id: &str, modulation: Modulation) {
        if modulation.source == ModSource::Constant || modulation.amount == 0.0 {
            self.map.remove(id);
        } else {
            self.map.insert(id.to_owned(), modulation);
        }
    }

    pub fn clear(&mut self, id: &str) {
        self.map.remove(id);
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, Modulation)> {
        self.map.iter().map(|(id, m)| (id.as_str(), *m))
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    const SCHEMA: &[ParamDef] = &[
        ParamDef {
            id: "length",
            label: "Length",
            group: "Shape",
            tooltip: "",
            kind: ParamKind::Float {
                min: 1.0,
                max: 100.0,
                default: 40.0,
                step: 1.0,
            },
            modulatable: true,
            exposed: true,
        },
        ParamDef {
            id: "layers",
            label: "Layers",
            group: "Depth",
            tooltip: "",
            kind: ParamKind::Int {
                min: 1,
                max: 4,
                default: 2,
            },
            modulatable: false,
            exposed: true,
        },
        ParamDef {
            id: "solid",
            label: "Solid",
            group: "Shape",
            tooltip: "",
            kind: ParamKind::Bool { default: true },
            modulatable: false,
            exposed: true,
        },
        ParamDef {
            id: "style",
            label: "Style",
            group: "Shape",
            tooltip: "",
            kind: ParamKind::Choice {
                options: &["A", "B", "C"],
                default: 1,
            },
            modulatable: false,
            exposed: true,
        },
    ];

    #[test]
    fn missing_values_fall_back_to_defaults() {
        let params = Params::default();
        let r = Resolved::new(SCHEMA, &params);
        assert_eq!(r.float("length"), 40.0);
        assert_eq!(r.int("layers"), 2);
        assert!(r.bool("solid"));
        assert_eq!(r.choice("style"), 1);
    }

    #[test]
    fn from_schema_materialises_every_default() {
        let params = Params::from_schema(SCHEMA);
        assert_eq!(params.len(), SCHEMA.len());
        let r = Resolved::new(SCHEMA, &params);
        assert_eq!(r.float("length"), 40.0);
    }

    // A document written when the slider went to 500 must survive the range
    // being tightened later.
    #[test]
    fn out_of_range_values_clamp() {
        let mut params = Params::default();
        params.set_float("length", 500.0);
        params.set_int("layers", -3);
        params.set_choice("style", 99);
        let r = Resolved::new(SCHEMA, &params);
        assert_eq!(r.float("length"), 100.0);
        assert_eq!(r.int("layers"), 1);
        assert_eq!(r.choice("style"), 2);
    }

    #[test]
    fn wrong_type_and_unknown_ids_do_not_panic() {
        let mut params = Params::default();
        params.set_bool("length", true);
        let r = Resolved::new(SCHEMA, &params);
        assert_eq!(r.float("length"), 40.0, "bad type falls back to the default");
        assert_eq!(r.float("nonexistent"), 0.0);
        assert!(!r.bool("nonexistent"));
    }

    #[test]
    fn non_finite_floats_fall_back() {
        let mut params = Params::default();
        params.set_float("length", f32::NAN);
        let r = Resolved::new(SCHEMA, &params);
        assert_eq!(r.float("length"), 40.0);
    }

    #[test]
    fn prune_drops_stale_ids_only() {
        let mut params = Params::from_schema(SCHEMA);
        params.set_float("removed_last_release", 1.0);
        assert_eq!(params.len(), SCHEMA.len() + 1);
        params.prune(SCHEMA);
        assert_eq!(params.len(), SCHEMA.len());
        assert!(params.get("length").is_some());
    }

    #[test]
    fn modulation_amount_one_hands_the_knob_to_the_source() {
        let m = Modulation::new(ModSource::Pressure, 1.0);
        assert_eq!(m.factor(1.0), 1.0, "full pressure gives the knob value");
        assert_eq!(m.factor(0.0), 0.0, "no pressure gives nothing");
        assert_eq!(m.factor(0.5), 0.5);
    }

    #[test]
    fn modulation_amount_zero_is_the_identity() {
        let m = Modulation::new(ModSource::Pressure, 0.0);
        for i in 0..=10 {
            assert_eq!(m.factor(i as f32 / 10.0), 1.0);
        }
    }

    #[test]
    fn negative_amount_inverts_the_source() {
        let m = Modulation::new(ModSource::Pressure, -1.0);
        assert_eq!(m.factor(0.0), 1.0);
        assert_eq!(m.factor(1.0), 0.0);
    }

    #[test]
    fn constant_source_never_modulates() {
        let m = Modulation::new(ModSource::Constant, 1.0);
        assert_eq!(m.factor(0.0), 1.0);
    }

    #[test]
    fn binding_a_constant_clears_the_entry() {
        let mut b = Bindings::default();
        b.set("length", Modulation::new(ModSource::Pressure, 0.8));
        assert!(!b.is_empty());
        b.set("length", Modulation::new(ModSource::Constant, 1.0));
        assert!(b.is_empty(), "a Constant binding is stored as no binding");
    }

    #[test]
    fn params_round_trip_through_serde() {
        let mut params = Params::from_schema(SCHEMA);
        params.set_float("length", 12.5);
        let json = serde_json::to_string(&params).expect("serialise");
        let back: Params = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(params, back);
    }
}
