//! One file per widget primitive. The widget vocabulary is closed
//! (rows, key_value, text, timeline, scene, sections); a new file appearing
//! here means a new primitive — which should set off the same alarm as a new
//! widget in the engine. `scene` is the generic scene renderer (backdrop,
//! atlas, any future scene); `sections` is the foldable multi-section list;
//! key_value/text gain files when they get renderers of their own; today they
//! degrade through lines.

pub mod rows;
pub mod scene;
pub mod sections;
pub mod timeline;
