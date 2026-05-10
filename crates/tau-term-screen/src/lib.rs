pub mod screen;
pub mod style;

pub use screen::{Screen, emit_styled_cells, layout_block, layout_lines};
pub use style::{Align, BlockId, Cell, Color, Span, Style, StyledBlock, StyledText};
