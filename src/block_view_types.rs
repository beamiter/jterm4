/// Block view types module
///
/// Extracted type definitions from block_view.rs for better organization
/// and testability.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use gtk4::gdk::RGBA;
use lru::LruCache;
use std::num::NonZeroUsize;

/// Block state machine states
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockState {
    /// Idle, waiting for prompt
    Idle,
    /// Collecting prompt output
    CollectingPrompt,
    /// Prompt complete, awaiting command input
    AwaitingCommand,
    /// Collecting command output
    CollectingOutput,
    /// Alt screen mode (full screen app)
    AltScreen,
}

/// Data for a single command block
#[derive(Clone, Debug)]
pub struct BlockData {
    pub prompt: String,
    pub command: String,
    pub output: String,
    pub exit_code: Option<i32>,
}

/// A finished (non-editable) command block
#[derive(Clone, Debug)]
pub struct FinishedBlock {
    pub prompt: String,
    pub command: String,
    pub output: String,
    pub exit_code: Option<i32>,
}

/// ANSI style state for text rendering
#[derive(Clone, Debug)]
pub struct AnsiStyleState {
    pub fg: Option<RGBA>,
    pub bg: Option<RGBA>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

/// A run of text with consistent ANSI styling
#[derive(Clone, Debug)]
pub struct AnsiTextRun {
    pub text: String,
    pub style: AnsiStyleState,
}

/// Viewport state for virtual scrolling
#[derive(Clone, Debug)]
pub struct ViewportState {
    pub scroll_offset: f64,
    pub visible_height: f64,
    pub total_height: f64,
}

/// Widget pool for reusing block widgets
pub struct WidgetPool {
    available: Vec<gtk4::Box>,
    max_pool_size: usize,
}

impl WidgetPool {
    pub fn new(max_size: usize) -> Self {
        WidgetPool {
            available: Vec::new(),
            max_pool_size: max_size,
        }
    }

    pub fn acquire(&mut self) -> Option<gtk4::Box> {
        self.available.pop()
    }

    pub fn release(&mut self, widget: gtk4::Box) {
        if self.available.len() < self.max_pool_size {
            self.available.push(widget);
        }
    }
}

/// Scroll debouncer for efficient scroll updates
pub struct ScrollDebouncer {
    dirty: Rc<Cell<bool>>,
}

impl ScrollDebouncer {
    pub fn new() -> Self {
        ScrollDebouncer {
            dirty: Rc::new(Cell::new(false)),
        }
    }

    pub fn mark_dirty(&self, _scroll_rc: &gtk4::ScrolledWindow) {
        self.dirty.set(true);
    }
}

impl Default for ScrollDebouncer {
    fn default() -> Self {
        Self::new()
    }
}
