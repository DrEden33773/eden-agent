//! Interactive terminal frontend. The shared host owns execution; this crate owns local UI state.
mod app;
mod autocomplete;
mod clipboard;
mod context;
mod extensions;
mod fields;
mod forms;
mod image_preview;
mod input;
mod management;
mod model;
mod plugin;
mod projection;
mod references;
mod selection;
mod storage;
mod summary;
mod terminal;
mod text;
mod transcript_images;
mod view;

pub use terminal::{Options, run};

mod grok_diff;
mod grok_render;
