//! TUI rendering layer. Reads [`crate::state::AppState`] (via mpsc
//! messages) and renders with ratatui. Never talks to the network
//! directly; all side effects travel through [`crate::state::Action`].

pub mod app;
pub mod images;
pub mod screens;
pub mod theme;
pub mod widgets;

pub use app::run;
pub use theme::Theme;
