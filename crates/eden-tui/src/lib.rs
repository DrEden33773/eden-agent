//! Interactive terminal frontend. The shared host owns execution; this crate owns local UI state.
mod app;
mod autocomplete;
mod clipboard;
mod extensions;
mod fields;
mod forms;
mod input;
mod model;
mod plugin;
mod projection;
mod selection;
mod storage;
mod summary;
mod terminal;
mod text;
mod view;

pub use terminal::{Options, run};
