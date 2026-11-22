//! The shared layout table: every measure a renderer lays a view out with.
//!
//! One `const` value, [`LAYOUT`], read by the pixel renderer (Rune) and by the
//! terminal renderer. A renderer holds no layout number of its own: a role
//! width, a drop order, a row height, an inset, a bubble or column width, a
//! type size or a gap is a field here, so two renderers that draw the same view
//! model agree by construction and a change to a measure is made once.
//!
//! The table has two measure spaces, kept apart because their units differ:
//!
//! - [`PixelTable`] (`LAYOUT.px`) is logical pixels, as `f32`.
//! - [`CellTable`] (`LAYOUT.cells`) is character cells, as `usize`.
//!
//! Inside a space the fields are grouped by the role they size (a tile frame, a
//! table, a timeline, a dock card), never by the renderer that reads them. The
//! table holds numbers and small pure rules over them (a column's width for its
//! role, a column's drop rank); it holds no colour, no wording and no signed
//! data. Signed data says what a region does when space runs out; the numbers
//! it resolves to live here.
//!
//! Growing the table: a new role is a new group in the space it is measured in;
//! a group gains fields without any reader changing, because readers name the
//! fields they use. A per-column signed priority replaces [`ColumnTable::drop_rank`]'s
//! role ranking by passing the signed value through the same method; a
//! breakpoint table (the widths at which a page changes shape, and the steps
//! its type and spacing shrink by) is one more group of `f32` and `usize`
//! fields beside the existing ones.

use crate::ui::content::ColumnRole;

/// The inset and spacing of a scrolling list body.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ListInset {
    /// Left and right.
    pub x: f32,
    pub top: f32,
    pub bottom: f32,
    /// Between rows.
    pub gap: f32,
    /// Rows sit in a centred column no wider than this.
    pub column: Option<f32>,
}

/// A font size with its weight and letter spacing, in the table's type scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TypeStep {
    pub size: f32,
    pub weight: f32,
    /// Letter spacing in px; `0.0` for none.
    pub tracking: f32,
}

const fn step(size: f32, weight: f32) -> TypeStep {
    TypeStep {
        size,
        weight,
        tracking: 0.0,
    }
}

/// How wide a column is: a fixed width, or a share of the room left over.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ColumnWidth {
    Fixed(f32),
    Grow {
        min: f32,
        weight: f32,
        max: Option<f32>,
    },
}

impl ColumnWidth {
    /// The least room the column needs.
    pub fn least(self) -> f32 {
        match self {
            Self::Fixed(width) => width,
            Self::Grow { min, .. } => min,
        }
    }
}

/// Column widths by role, and the order columns leave in when a tile is too
/// narrow for all of them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColumnTable {
    pub id: f32,
    pub kind: f32,
    pub status: f32,
    pub hash: f32,
    pub count: f32,
    pub action: f32,
    /// A time column takes the room left over, but never less than this.
    pub time_least: f32,
    pub time_weight: f32,
    /// A ref or path column beside a time column.
    pub ref_fixed: f32,
    /// A ref or path column that takes the room instead, and its share.
    pub ref_least: f32,
    pub ref_weight: f32,
    /// A text column's least width, and the widest it grows beside a ref or
    /// path column, which takes the room instead.
    pub text_least: f32,
    pub text_most: f32,
    pub text_weight: f32,
    /// Characters of an id shown before it is cut in the middle, and of a hash.
    pub id_chars: usize,
    pub hash_chars: usize,
    /// Which of two field-pair choices a narrow table makes. When true, a
    /// table keeps what a row is (its kind and item) and drops its id and time
    /// first; when false it keeps which run and when (id and time) and drops
    /// kind and item first.
    pub keep_kind_and_item: bool,
}

impl ColumnTable {
    /// The width a role asks for. Ids, kinds, statuses, counts and hashes are
    /// fixed so rows line up. A time column takes the room left over; refs and
    /// paths then hold a fixed width and text columns a short one, so slack
    /// gathers at the table's end rather than in the middle. With no time
    /// column, refs and paths take the room instead, with text columns beside
    /// them held to a bounded share (or sharing it equally with no ref or
    /// path).
    pub fn width(&self, role: ColumnRole, has_ref: bool, has_time: bool) -> ColumnWidth {
        match role {
            ColumnRole::Id => ColumnWidth::Fixed(self.id),
            ColumnRole::Kind => ColumnWidth::Fixed(self.kind),
            ColumnRole::Status => ColumnWidth::Fixed(self.status),
            ColumnRole::Time => ColumnWidth::Grow {
                min: self.time_least,
                weight: self.time_weight,
                max: None,
            },
            ColumnRole::Hash => ColumnWidth::Fixed(self.hash),
            ColumnRole::Count => ColumnWidth::Fixed(self.count),
            ColumnRole::Action => ColumnWidth::Fixed(self.action),
            ColumnRole::Ref | ColumnRole::Path if has_time => ColumnWidth::Fixed(self.ref_fixed),
            ColumnRole::Ref | ColumnRole::Path => ColumnWidth::Grow {
                min: self.ref_least,
                weight: self.ref_weight,
                max: None,
            },
            ColumnRole::Text if has_time => ColumnWidth::Fixed(self.text_least),
            ColumnRole::Text => ColumnWidth::Grow {
                min: self.text_least,
                weight: self.text_weight,
                max: has_ref.then_some(self.text_most),
            },
        }
    }

    /// When a tile is too narrow for every column, columns go in this order,
    /// lowest first; `None` never goes. A status says what happened to a row
    /// and never goes. With `keep_kind_and_item` an id goes after the text
    /// columns, since the item says what the row is, and a time goes first of
    /// the data columns.
    pub fn drop_rank(&self, role: ColumnRole) -> Option<u8> {
        match (role, self.keep_kind_and_item) {
            (ColumnRole::Hash, _) => Some(0),
            (ColumnRole::Time, true) => Some(1),
            (ColumnRole::Count, _) => Some(2),
            (ColumnRole::Text, _) => Some(3),
            (ColumnRole::Id, true) => Some(3),
            (ColumnRole::Kind, true) => Some(4),
            (ColumnRole::Path, true) => Some(5),
            (ColumnRole::Ref, true) => Some(6),
            // Kind and item leave before the id and the time.
            (ColumnRole::Kind, false) => Some(1),
            (ColumnRole::Path, false) => Some(1),
            (ColumnRole::Ref, false) => Some(1),
            (ColumnRole::Time, false) => Some(5),
            (ColumnRole::Id, false) => Some(6),
            (ColumnRole::Status | ColumnRole::Action, _) => None,
        }
    }
}

/// The type scale: sizes, weights and tracking, by step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TypeScale {
    /// The launcher's find line.
    pub find: TypeStep,
    /// Prose, message, composer placeholder.
    pub prose: TypeStep,
    /// Tile title, dock title.
    pub title: TypeStep,
    /// One-line entry headline and detail.
    pub line: TypeStep,
    /// Section values.
    pub cell: TypeStep,
    /// Table cells, state words, field names.
    pub small: TypeStep,
    /// Chips, key hints, usage.
    pub micro: TypeStep,
    /// Coverage sentences.
    pub note: TypeStep,
    /// A key chip's text.
    pub key: TypeStep,
    /// Column headers and section titles.
    pub label: TypeStep,
    /// A record's primary line.
    pub record: TypeStep,
    /// The regular weight of a size set without a step, and the medium weight
    /// for emphasis within a step.
    pub regular_weight: f32,
    pub medium_weight: f32,
    /// Sizes the authored heading, supplement and empty states use directly,
    /// in the interface and data faces.
    pub s9: f32,
    pub s10: f32,
    pub s11: f32,
    pub s12: f32,
    pub s13: f32,
    pub s32: f32,
    pub s38: f32,
    /// Letter spacing of an eyebrow, a supplement caption, an unresolved-view
    /// eyebrow and a display title.
    pub tracking_eyebrow: f32,
    pub tracking_caption: f32,
    pub tracking_unresolved: f32,
    pub tracking_display: f32,
}

/// Icon sizes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IconSizes {
    /// The tile's actions control.
    pub tile_more: f32,
    /// A section's fold mark.
    pub section_fold: f32,
    /// A record's disclosure triangle.
    pub disclosure: f32,
    /// A hierarchical row's fold mark, and a chevron.
    pub fold: f32,
}

/// Corner radii, tile gap and tile padding.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spacing {
    pub radius_tile: f32,
    pub radius_card: f32,
    pub radius_bubble: f32,
    pub radius_row: f32,
    pub radius_button: f32,
    pub radius_key: f32,
    /// Between tiles.
    pub gap_tile: f32,
    /// Inside a tile, horizontal.
    pub pad_tile: f32,
}

/// The shell's bands: tabs, switcher, status bar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bands {
    pub tabs_height: f32,
    pub tabs_top: f32,
    pub switcher_band_height: f32,
    pub status_bar_height: f32,
    pub island_height: f32,
    pub switcher_width: f32,
    pub switcher_rename_width: f32,
    pub switcher_chip: f32,
    pub switcher_inset: f32,
    pub switcher_room: f32,
    pub radius_switcher: f32,
    pub radius_sheet: f32,
}

/// A tile's frame: its header, tabs and tools, and the authored heading.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TileTable {
    pub title_row_height: f32,
    pub title_gap: f32,
    pub tools_gap: f32,
    pub maximize_pad: f32,
    pub more_pad: f32,
    /// The tab row of a grouped tile.
    pub group_tabs_min_height: f32,
    pub group_tabs_px: f32,
    pub tab_px: f32,
    pub tab_indent: f32,
    /// How much of the tile fill a tile with a transparent background keeps.
    pub transparent_fill: f32,
    /// The project-authorship chip.
    pub chip_height: f32,
    pub chip_px: f32,
    pub chip_gap: f32,
    pub chip_divider_height: f32,
    pub chip_border: f32,
    pub chip_divider: f32,
    pub chip_name: f32,
    /// The authored heading above a view.
    pub heading_pt: f32,
    pub heading_px: f32,
    pub heading_pb: f32,
    pub heading_title_max_width: f32,
    pub heading_title_mt: f32,
    pub heading_title_mb: f32,
    pub heading_summary_max_width: f32,
    pub heading_metadata_mt: f32,
}

/// The supplement excerpt and footer, and the tile actions menu.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SupplementTable {
    pub caption_px: f32,
    pub caption_py: f32,
    pub excerpt_min_height: f32,
    pub excerpt_px: f32,
    pub excerpt_py: f32,
    pub excerpt_field_width: f32,
    pub excerpt_good_fill: f32,
    pub excerpt_max_height: f32,
    pub footer_py: f32,
    pub footer_row_px: f32,
    pub footer_row_py: f32,
    pub footer_row_gap: f32,
    pub menu_offset: f32,
    pub menu_min_width: f32,
    pub menu_max_width: f32,
    pub menu_pad: f32,
    pub menu_item_px: f32,
    pub menu_item_py: f32,
}

/// List, section, record and document bodies.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ListTable {
    /// The list views' inset.
    pub inset: ListInset,
    /// Padding of a scrolling list body (both sides).
    pub pad_x: f32,
    pub pad_y: f32,
    /// Lists longer than this build only the rows in view.
    pub virtual_rows_from: usize,
    /// Estimated heights for rows not yet measured.
    pub row_estimate: f32,
    pub table_row_estimate: f32,
    pub timeline_entry_estimate: f32,
    /// A section body's vertical padding, and what its padding gives back to a
    /// field row's inset.
    pub section_py: f32,
    pub section_title_above: f32,
    pub section_title_below: f32,
    pub section_header_gap: f32,
    /// A section's field row.
    pub field_row_height: f32,
    pub field_row_gap: f32,
    pub field_name_width: f32,
    pub field_inset: f32,
    /// A tone's dot, and the gap between it and its word.
    pub tone_dot: f32,
    pub tone_dot_gap: f32,
    /// A record.
    pub record_copy_gap: f32,
    pub record_min_height: f32,
    pub record_px: f32,
    pub record_py: f32,
    pub record_glyph_width: f32,
    pub record_disclosure_width: f32,
    /// The opacity of an unavailable record.
    pub unavailable_opacity: f32,
    /// A text view.
    pub text_px: f32,
    pub text_py: f32,
    pub text_gap: f32,
    /// A document view.
    pub doc_header_px: f32,
    pub doc_header_py: f32,
    pub doc_header_gap: f32,
    pub doc_pt: f32,
    pub doc_px: f32,
    pub doc_pb: f32,
}

/// A table's rows and heading row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TableTable {
    /// The disclosure column of a table whose rows do not open on activation.
    pub action_width: f32,
    pub header_height: f32,
    pub header_ink: f32,
    pub row_height: f32,
    /// A hierarchy table's row.
    pub row_height_tree: f32,
    pub row_pad: f32,
    pub cell_gap: f32,
    pub row_gap: f32,
    /// Rows are inset from the tile so a selection fill has a margin.
    pub inset: f32,
    pub pad_bottom: f32,
    /// Indent per level of a hierarchical table, and the fold mark's place.
    pub indent: f32,
    pub fold_mark_width: f32,
    pub fold_pad: f32,
    pub cell_py: f32,
    /// The coverage sentence foot.
    pub foot_py: f32,
    /// The table body's padding: rows inset from the tile.
    pub list_inset: ListInset,
    pub columns: ColumnTable,
}

/// The timeline feed, its status lane and dock, and its lines.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimelineTable {
    /// Indent per call-tree level.
    pub indent: f32,
    /// Content sits in a column no wider than this, centred in the tile.
    pub column: f32,
    pub pad_x: f32,
    pub pad_top: f32,
    /// The feed's viewport never gets shorter than this.
    pub feed_floor: f32,
    /// The space above the status lane.
    pub lane_space: f32,
    pub fade: f32,
    pub gap: f32,
    /// A selected line's fill reaches this far past the column.
    pub select_reach: f32,
    pub chevron_width: f32,
    pub line_height: f32,
    pub marker_height: f32,
    pub line_gap: f32,
    pub marker_gap: f32,
    /// The least width a one-line entry's detail keeps.
    pub detail_floor: f32,
    /// A message bubble.
    pub bubble_width: f32,
    pub bubble_wrap: f32,
    pub bubble_pad_x: f32,
    pub bubble_pad_y: f32,
    pub bubble_fill: f32,
    /// The status lane.
    pub lane_height: f32,
    pub lane_dot: f32,
    pub lane_gap: f32,
    /// A dock card.
    pub dock_stack_max: f32,
    pub dock_pad: f32,
    pub dock_gap: f32,
    pub dock_stack_gap: f32,
    pub dock_title_gap: f32,
    pub dock_border: f32,
    pub dock_fill: f32,
    pub dock_button_height: f32,
    pub dock_button_pad: f32,
    pub dock_key_gap: f32,
    pub dock_key_alpha: f32,
    /// A key's cap.
    pub key_cap_height: f32,
    pub key_cap_min_width: f32,
    pub key_cap_px: f32,
    pub key_cap_border: f32,
    pub key_cap_ink: f32,
    /// The feed's list padding: the column is centred, and a selected line's
    /// fill reaches past it on either side.
    pub list_inset: ListInset,
}

/// Field/value pairs under an expanded record or in a floating panel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PairsTable {
    pub record_label_width: f32,
    pub panel_label_width: f32,
    pub panel_gap: f32,
    pub record_pt: f32,
    pub record_pb: f32,
    pub record_pl: f32,
    pub record_pr: f32,
    pub panel_py: f32,
    pub row_py: f32,
}

/// Empty, unavailable, loading and unresolved states.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StateTable {
    pub pad_x: f32,
    pub pad_top: f32,
    pub status_py: f32,
    pub unavailable_gap: f32,
    pub unavailable_row_gap: f32,
    pub unavailable_row_min_height: f32,
    pub unavailable_dot: f32,
    /// The skeleton's bars in a row's columns, and its rows.
    pub skeleton_bars: [f32; 6],
    pub skeleton_rows: usize,
    pub skeleton_row_height: f32,
    pub skeleton_bar_height: f32,
    pub skeleton_fill: f32,
    pub skeleton_row_gap: f32,
    pub skeleton_line_gap: f32,
    pub skeleton_gap: f32,
    pub skeleton_word_ink: f32,
    /// A view waiting for a subject.
    pub required_gap: f32,
    pub required_pad: f32,
    pub required_body_max_width: f32,
    pub required_choices_gap: f32,
    pub required_choices_mt: f32,
    pub required_choice_px: f32,
    pub required_choice_py: f32,
}

/// The overlay sheet (the find and place panel).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayTable {
    pub sheet_width: f32,
    pub sheet_top: f32,
    pub sheet_radius: f32,
    pub row_height: f32,
    pub row_radius: f32,
    pub find_height: f32,
    pub card_width: f32,
    pub card_height: f32,
    /// A window narrower than this cannot hold the sheet at its width with
    /// four cards in a row (four cards, three gaps, the sheet's padding and
    /// border, and 16 either side): the cards wrap and the two work columns
    /// stack.
    pub tight_width: f32,
}

/// The launcher sheet.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LauncherTable {
    pub sheet_width: f32,
    pub sheet_max_height: f32,
    pub find_height: f32,
    pub row_pad_x: f32,
    pub row_pad_y: f32,
    pub row_gap: f32,
    pub row_radius: f32,
    pub row_height: f32,
    pub command_label_ink: f32,
    pub command_name_ink: f32,
    pub name_width: f32,
    pub place_height: f32,
    pub place_radius: f32,
    pub foot_height: f32,
    pub dimmed: f32,
    pub thumb_width: f32,
    pub thumb_height: f32,
}

/// The shell's own metrics: the composer, the split dividers, the tag.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShellTable {
    pub tag_height: f32,
    pub tabs_fade: f32,
    pub side_reserve: f32,
    pub min_pane: f32,
    pub split_step: f32,
    pub split_gap: f32,
    pub narrow_hints: usize,
    pub composer_column: f32,
    pub composer_pad: f32,
    pub composer_gap: f32,
    pub composer_fill: f32,
    pub chip_height: f32,
    pub send_size: f32,
    pub pending_border: f32,
    pub pending_dot: f32,
    pub filter_height: f32,
}

/// The field view's graph metrics.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FieldTable {
    pub group_width: f32,
    pub empty_group_width: f32,
    pub rank_spacing: f32,
    pub group_right_gutter: f32,
    pub group_header_height: f32,
    pub transcript_rows: u32,
    pub key_figure_cells: usize,
}

/// Every pixel measure.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PixelTable {
    pub type_scale: TypeScale,
    pub icons: IconSizes,
    pub spacing: Spacing,
    pub bands: Bands,
    pub tile: TileTable,
    pub supplement: SupplementTable,
    pub list: ListTable,
    pub table: TableTable,
    pub timeline: TimelineTable,
    pub pairs: PairsTable,
    pub state: StateTable,
    pub overlay: OverlayTable,
    pub launcher: LauncherTable,
    pub shell: ShellTable,
    pub field: FieldTable,
}

/// The terminal's measures, in character cells.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellTable {
    /// The switcher strip: the gap between entries.
    pub strip_gap: usize,
    /// A dialog's widest and tallest size, and what it leaves of the screen's
    /// height above and below, and the least it is drawn at.
    pub dialog_max_width: usize,
    pub dialog_max_height: usize,
    pub dialog_height_margin: usize,
    pub dialog_least: usize,
    /// The dialog's input is at most this many rows.
    pub dialog_input_max_rows: usize,
    /// The dialog sits this fraction down (as a divisor) of the room left.
    pub dialog_top_divisor: usize,
    /// A tile's composer is at most this many rows.
    pub tile_input_max_rows: u16,
    /// Table cells sit this far in, past the tone-glyph gutter.
    pub table_gutter: usize,
    /// The least height at which a table's coverage foot is drawn.
    pub table_foot_min_height: usize,
    /// Indent per level of a hierarchical table.
    pub table_indent: &'static str,
    /// A section row's indent.
    pub section_indent: usize,
    /// Most pending asks docked under a timeline.
    pub max_docked: usize,
    /// A timeline builds this many lines of entries per row of height, and at
    /// least this many.
    pub timeline_window_per_row: usize,
    pub timeline_window_least: usize,
    /// Columns of indent per call-tree level in a timeline.
    pub timeline_indent: usize,
    /// The field view's map: the least width that shows it, its share of the
    /// width as a divisor, and its least width.
    pub field_grid_min_width: usize,
    pub field_grid_divisor: usize,
    pub field_grid_least: usize,
    /// Most warnings, metrics and subjects the field footer lists.
    pub field_warnings: usize,
    pub field_metrics: usize,
    pub field_subjects: usize,
    /// Most nesting levels the field list indents, and the rail window.
    pub field_depth: usize,
    pub field_rail_window: usize,
    pub field_rail_lead: usize,
    /// A list's meta column takes at most this share of the width, as a
    /// divisor.
    pub meta_divisor: usize,
}

/// The whole table.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayoutTable {
    pub px: PixelTable,
    pub cells: CellTable,
}

// Values shared by more than one field.
const PAD_TILE: f32 = 18.0;
const SELECT_REACH: f32 = 10.0;
const TIMELINE_PAD_X: f32 = 24.0;
const TIMELINE_COLUMN: f32 = 720.0;
const TIMELINE_GAP: f32 = 16.0;
const TABLE_INSET: f32 = 8.0;
const TABLE_ROW_GAP: f32 = 1.0;
const LIST_PAD_X: f32 = 16.0;
const LIST_PAD_Y: f32 = 12.0;

/// Every layout measure both renderers read.
pub const LAYOUT: LayoutTable = LayoutTable {
    px: PixelTable {
        type_scale: TypeScale {
            find: step(19.0, 400.0),
            prose: step(15.0, 400.0),
            title: step(13.5, 500.0),
            line: step(13.0, 400.0),
            cell: step(12.5, 400.0),
            small: step(12.0, 400.0),
            micro: step(11.5, 400.0),
            note: step(11.0, 400.0),
            key: step(10.5, 500.0),
            label: TypeStep {
                size: 10.5,
                weight: 500.0,
                tracking: 1.5,
            },
            record: step(13.0, 450.0),
            regular_weight: 400.0,
            medium_weight: 500.0,
            s9: 9.0,
            s10: 10.0,
            s11: 11.0,
            s12: 12.0,
            s13: 13.0,
            s32: 32.0,
            s38: 38.0,
            tracking_eyebrow: 1.3,
            tracking_caption: 0.7,
            tracking_unresolved: 1.2,
            tracking_display: -1.5,
        },
        icons: IconSizes {
            tile_more: 14.0,
            section_fold: 10.0,
            disclosure: 13.0,
            fold: 12.0,
        },
        spacing: Spacing {
            radius_tile: 16.0,
            radius_card: 12.0,
            radius_bubble: 14.0,
            radius_row: 8.0,
            radius_button: 9.0,
            radius_key: 5.0,
            gap_tile: 12.0,
            pad_tile: PAD_TILE,
        },
        bands: Bands {
            tabs_height: 50.0,
            tabs_top: 6.0,
            switcher_band_height: 40.0,
            status_bar_height: 40.0,
            island_height: 30.0,
            switcher_width: 40.0,
            switcher_rename_width: 260.0,
            switcher_chip: 28.0,
            switcher_inset: 10.0,
            switcher_room: 44.0,
            radius_switcher: 14.0,
            radius_sheet: 22.0,
        },
        tile: TileTable {
            title_row_height: 44.0,
            title_gap: 10.0,
            tools_gap: 8.0,
            maximize_pad: 2.0,
            more_pad: 5.0,
            group_tabs_min_height: 40.0,
            group_tabs_px: 6.0,
            tab_px: 13.0,
            tab_indent: 18.0,
            transparent_fill: 0.8,
            chip_height: 22.0,
            chip_px: 9.0,
            chip_gap: 7.0,
            chip_divider_height: 10.0,
            chip_border: 0.22,
            chip_divider: 0.18,
            chip_name: 0.85,
            heading_pt: 28.0,
            heading_px: 34.0,
            heading_pb: 22.0,
            heading_title_max_width: 720.0,
            heading_title_mt: 14.0,
            heading_title_mb: 10.0,
            heading_summary_max_width: 620.0,
            heading_metadata_mt: 17.0,
        },
        supplement: SupplementTable {
            caption_px: 12.0,
            caption_py: 8.0,
            excerpt_min_height: 20.0,
            excerpt_px: 12.0,
            excerpt_py: 2.0,
            excerpt_field_width: 42.0,
            excerpt_good_fill: 0.09,
            excerpt_max_height: 240.0,
            footer_py: 4.0,
            footer_row_px: 14.0,
            footer_row_py: 5.0,
            footer_row_gap: 12.0,
            menu_offset: 2.0,
            menu_min_width: 180.0,
            menu_max_width: 360.0,
            menu_pad: 4.0,
            menu_item_px: 10.0,
            menu_item_py: 9.0,
        },
        list: ListTable {
            inset: ListInset {
                x: LIST_PAD_X,
                top: LIST_PAD_Y,
                bottom: LIST_PAD_Y,
                gap: 0.0,
                column: None,
            },
            pad_x: LIST_PAD_X,
            pad_y: LIST_PAD_Y,
            virtual_rows_from: 64,
            row_estimate: 46.0,
            table_row_estimate: 38.0,
            timeline_entry_estimate: 44.0,
            section_py: 8.0,
            section_title_above: 12.0,
            section_title_below: 9.0,
            section_header_gap: 8.0,
            field_row_height: 24.0,
            field_row_gap: 8.0,
            field_name_width: 104.0,
            field_inset: 8.0,
            tone_dot: 6.0,
            tone_dot_gap: 7.0,
            record_copy_gap: 3.0,
            record_min_height: 43.0,
            record_px: 10.0,
            record_py: 7.0,
            record_glyph_width: 24.0,
            record_disclosure_width: 34.0,
            unavailable_opacity: 0.5,
            text_px: 28.0,
            text_py: 22.0,
            text_gap: 8.0,
            doc_header_px: 16.0,
            doc_header_py: 9.0,
            doc_header_gap: 16.0,
            doc_pt: 18.0,
            doc_px: 20.0,
            doc_pb: 32.0,
        },
        table: TableTable {
            action_width: 24.0,
            header_height: 28.0,
            header_ink: 0.40,
            row_height: 28.0,
            row_height_tree: 26.0,
            row_pad: 10.0,
            cell_gap: 12.0,
            row_gap: TABLE_ROW_GAP,
            inset: TABLE_INSET,
            pad_bottom: 8.0,
            indent: 16.0,
            fold_mark_width: 16.0,
            fold_pad: 2.0,
            cell_py: 6.0,
            foot_py: 11.0,
            list_inset: ListInset {
                x: TABLE_INSET,
                top: 0.0,
                bottom: 8.0,
                gap: TABLE_ROW_GAP,
                column: None,
            },
            columns: ColumnTable {
                id: 112.0,
                kind: 78.0,
                status: 104.0,
                hash: 84.0,
                count: 56.0,
                action: 24.0,
                time_least: 100.0,
                time_weight: 1.0,
                ref_fixed: 150.0,
                ref_least: 150.0,
                ref_weight: 2.0,
                text_least: 80.0,
                text_most: 160.0,
                text_weight: 1.0,
                id_chars: 13,
                hash_chars: 8,
                keep_kind_and_item: true,
            },
        },
        timeline: TimelineTable {
            indent: 18.0,
            column: TIMELINE_COLUMN,
            pad_x: TIMELINE_PAD_X,
            pad_top: 14.0,
            feed_floor: 160.0,
            lane_space: 16.0,
            fade: 16.0,
            gap: TIMELINE_GAP,
            select_reach: SELECT_REACH,
            chevron_width: 24.0,
            line_height: 26.0,
            marker_height: 22.0,
            line_gap: 9.0,
            marker_gap: 12.0,
            detail_floor: 72.0,
            bubble_width: 502.0,
            bubble_wrap: 470.0,
            bubble_pad_x: 16.0,
            bubble_pad_y: 12.0,
            bubble_fill: 0.09,
            lane_height: 24.0,
            lane_dot: 7.0,
            lane_gap: 10.0,
            dock_stack_max: 260.0,
            dock_pad: 16.0,
            dock_gap: 10.0,
            dock_stack_gap: 8.0,
            dock_title_gap: 10.0,
            dock_border: 0.50,
            dock_fill: 0.05,
            dock_button_height: 32.0,
            dock_button_pad: 14.0,
            dock_key_gap: 8.0,
            dock_key_alpha: 0.5,
            key_cap_height: 20.0,
            key_cap_min_width: 20.0,
            key_cap_px: 6.0,
            key_cap_border: 0.16,
            key_cap_ink: 0.60,
            list_inset: ListInset {
                x: TIMELINE_PAD_X - SELECT_REACH,
                top: 0.0,
                bottom: 0.0,
                gap: TIMELINE_GAP,
                column: Some(TIMELINE_COLUMN + 2.0 * SELECT_REACH),
            },
        },
        pairs: PairsTable {
            record_label_width: 110.0,
            panel_label_width: 76.0,
            panel_gap: 8.0,
            record_pt: 7.0,
            record_pb: 9.0,
            record_pl: 34.0,
            record_pr: 12.0,
            panel_py: 5.0,
            row_py: 2.0,
        },
        state: StateTable {
            pad_x: 10.0,
            pad_top: 10.0,
            status_py: 6.0,
            unavailable_gap: 2.0,
            unavailable_row_gap: 10.0,
            unavailable_row_min_height: 26.0,
            unavailable_dot: 7.0,
            skeleton_bars: [90.0, 62.0, 120.0, 64.0, 83.0, 56.0],
            skeleton_rows: 8,
            skeleton_row_height: 28.0,
            skeleton_bar_height: 8.0,
            skeleton_fill: 0.07,
            skeleton_row_gap: 1.0,
            skeleton_line_gap: 12.0,
            skeleton_gap: 10.0,
            skeleton_word_ink: 0.40,
            required_gap: 12.0,
            required_pad: 48.0,
            required_body_max_width: 560.0,
            required_choices_gap: 8.0,
            required_choices_mt: 8.0,
            required_choice_px: 12.0,
            required_choice_py: 9.0,
        },
        overlay: OverlayTable {
            sheet_width: 920.0,
            sheet_top: 150.0,
            sheet_radius: 22.0,
            row_height: 46.0,
            row_radius: 10.0,
            find_height: 60.0,
            card_width: 206.0,
            card_height: 118.0,
            tight_width: 4.0 * 206.0 + 3.0 * 12.0 + 48.0 + 2.0 + 32.0,
        },
        launcher: LauncherTable {
            sheet_width: 860.0,
            sheet_max_height: 640.0,
            find_height: 64.0,
            row_pad_x: 14.0,
            row_pad_y: 10.0,
            row_gap: 14.0,
            row_radius: 11.0,
            row_height: 34.0,
            command_label_ink: 0.30,
            command_name_ink: 0.60,
            name_width: 190.0,
            place_height: 44.0,
            place_radius: 10.0,
            foot_height: 44.0,
            dimmed: 0.55,
            thumb_width: 46.0,
            thumb_height: 30.0,
        },
        shell: ShellTable {
            tag_height: 24.0,
            tabs_fade: 24.0,
            side_reserve: 32.0,
            min_pane: 400.0,
            split_step: 0.05,
            split_gap: 0.0,
            narrow_hints: 4,
            composer_column: 720.0,
            composer_pad: 14.0,
            composer_gap: 16.0,
            composer_fill: 0.55,
            chip_height: 24.0,
            send_size: 30.0,
            pending_border: 0.6,
            pending_dot: 5.0,
            filter_height: 30.0,
        },
        field: FieldTable {
            group_width: 520.0,
            empty_group_width: 200.0,
            rank_spacing: 210.0,
            group_right_gutter: 300.0,
            group_header_height: 26.0,
            transcript_rows: 16,
            key_figure_cells: 5,
        },
    },
    cells: CellTable {
        strip_gap: 1,
        dialog_max_width: 84,
        dialog_max_height: 32,
        dialog_height_margin: 4,
        dialog_least: 4,
        dialog_input_max_rows: 4,
        dialog_top_divisor: 3,
        tile_input_max_rows: 3,
        table_gutter: 2,
        table_foot_min_height: 4,
        table_indent: "  ",
        section_indent: 2,
        max_docked: 3,
        timeline_window_per_row: 4,
        timeline_window_least: 64,
        timeline_indent: 2,
        field_grid_min_width: 58,
        field_grid_divisor: 3,
        field_grid_least: 18,
        field_warnings: 3,
        field_metrics: 3,
        field_subjects: 2,
        field_depth: 8,
        field_rail_window: 5,
        field_rail_lead: 2,
        meta_divisor: 3,
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_role_asks_for_its_width_and_the_flexible_roles_grow() {
        let columns = LAYOUT.px.table.columns;
        for (role, width) in [
            (ColumnRole::Id, 112.0),
            (ColumnRole::Kind, 78.0),
            (ColumnRole::Status, 104.0),
            (ColumnRole::Hash, 84.0),
            (ColumnRole::Count, 56.0),
            (ColumnRole::Action, 24.0),
        ] {
            assert_eq!(
                columns.width(role, true, true),
                ColumnWidth::Fixed(width),
                "{role:?}"
            );
        }
        assert!(matches!(
            columns.width(ColumnRole::Time, true, true),
            ColumnWidth::Grow { .. }
        ));
        assert!(matches!(
            columns.width(ColumnRole::Text, true, false),
            ColumnWidth::Grow { max: Some(_), .. }
        ));
        assert!(matches!(
            columns.width(ColumnRole::Text, false, false),
            ColumnWidth::Grow { max: None, .. }
        ));
    }

    #[test]
    fn a_status_never_leaves_and_the_drop_order_follows_the_pair() {
        let mut columns = LAYOUT.px.table.columns;
        assert_eq!(columns.drop_rank(ColumnRole::Status), None);
        assert_eq!(columns.drop_rank(ColumnRole::Action), None);
        assert!(columns.drop_rank(ColumnRole::Time) < columns.drop_rank(ColumnRole::Id));
        columns.keep_kind_and_item = false;
        assert!(columns.drop_rank(ColumnRole::Kind) < columns.drop_rank(ColumnRole::Time));
    }

    #[test]
    fn the_list_inset_is_the_list_padding() {
        let list = LAYOUT.px.list;
        assert_eq!(list.inset.x, list.pad_x);
        assert_eq!(list.inset.top, list.pad_y);
        assert_eq!(list.inset.bottom, list.pad_y);
    }
}
