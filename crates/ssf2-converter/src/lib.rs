// Clippy baseline for this existing codebase: CI runs `cargo clippy -- -D warnings`,
// so anything not allowed here is a hard error. these families are pedantic-style or
// pre-existing patterns we accept; new code should still try to avoid them.
#![allow(
    clippy::doc_overindented_list_items,
    clippy::doc_lazy_continuation,
    clippy::empty_line_after_doc_comments,
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::manual_checked_ops,
    clippy::unnecessary_unwrap,
    clippy::neg_cmp_op_on_partial_ord,
    clippy::field_reassign_with_default,
    clippy::collapsible_match,
)]

// Public modules — exposed for use by binaries in src/bin/
pub mod ssf;
pub mod test_fixture;
pub mod uuid_gen;
pub mod swf_parser;
pub mod swf_timeline;
pub mod entity_compact;
pub mod abc_parser;
pub mod decompiler;
pub mod extractor;
pub mod anim_splitter;
pub mod entity_gen;
pub mod fraytools_project;
pub mod haxe_gen;
pub mod projectile_gen;
pub mod palette_gen;
pub mod sprite_parser;
pub mod image_extractor;
pub mod sound_extractor;
pub mod api_mappings;
pub mod mappings;
pub mod project;
pub mod fraytools_transform;
pub mod fm_placement;
pub mod vector_raster;
pub mod physics_sim;
pub mod abc_codec;
pub mod engine_probe;
pub mod abc_inject;

// In-process conversion entry point (was the `ssf2_converter` binary's main()).
pub mod convert;
pub use convert::{run_conversion, ConversionSummary, ConvertOptions};

// Asset classification (identify which `.ssf` is a character / stage / other).
pub mod classify;
pub use classify::{classify_ssf, AssetKind, SsfClassification};

// SSF2 stage AS3 reader (plane assignments + spawned actors) + geometry parser + emitter.
pub mod stage_abc;
pub mod stage_parser;
pub use stage_parser::{parse_stage, parse_stage_opts, ParallaxLayer, ParallaxMode, Platform, Rect, SpawnPoint, StageArt, StageArtSet, StageModel};
pub mod stage_emit;
pub mod content_audit;
pub use stage_emit::{emit_stage, render_placeholder_for_test};
