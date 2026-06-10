//! Scale-picker scene: the render-facing row contract.
//!
//! Already-projected display data — row labels only, in catalogue order.
//! Music-blind. Open/closed visibility is **not** here: that is a
//! route/interaction concern owned by glue (`ShowingScalePicker`). The
//! scene describes "given it's open, here is what to render."

/// One picker row: a display label. Its catalogue index is its position in
/// the [`PickerRows`] slice — the row marker stores that index so the click
/// handler can look the shape up in the catalogue.
#[derive(Debug, Clone, PartialEq)]
pub struct PickerRow {
    pub label: String,
}

/// The rows to render while the picker is open, in catalogue order.
pub type PickerRows = Vec<PickerRow>;
