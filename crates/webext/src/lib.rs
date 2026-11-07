//! Browser-independent core of a WebExtensions (Chrome/Firefox) compatibility layer.

pub mod alarms;
pub mod analyze;
pub mod bus;
pub mod content_scripts;
pub mod dispatch;
pub mod encoding;
pub mod host;
pub mod i18n;
pub mod loader;
pub mod manifest;
pub mod match_pattern;
pub mod package;
pub mod permissions;
pub mod registry;
pub mod render;
pub mod resources;
pub mod serve;
pub mod storage;
pub mod store;
pub mod zip;
