//! One file per widget primitive. The widget vocabulary is closed
//! (rows, key_value, text, timeline, scene); a new file appearing here
//! means a new primitive — which should set off the same alarm as a
//! sixth widget in the engine. key_value/text and scene gain files when
//! they get renderers of their own; today they degrade through lines.

pub mod rows;
pub mod timeline;
