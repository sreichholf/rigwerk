//! The saved theme and size override egui's own memory.  Keyboard zoom
//! is off, so Ctrl++, Ctrl+− and Ctrl+0 land here and are saved.

use eframe::egui;

use crate::app::RigWerkApp;

/// Which egui theme to draw with.  *System* follows the platform, and is
/// dark when the platform reports nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum ThemeChoice {
    Dark,
    Light,
    /// Also what a theme name this build does not know reads as.
    #[default]
    #[serde(other)]
    System,
}

impl ThemeChoice {
    pub(crate) fn preference(self) -> egui::ThemePreference {
        match self {
            Self::System => egui::ThemePreference::System,
            Self::Dark => egui::ThemePreference::Dark,
            Self::Light => egui::ThemePreference::Light,
        }
    }
}

/// The theme the window is drawn in: the run's override, else the saved
/// choice.  *System* follows the platform; egui falls back to dark.
pub(crate) fn drawn_theme(saved: ThemeChoice, run_override: Option<ThemeChoice>) -> ThemeChoice {
    run_override.unwrap_or(saved)
}

/// The interface size, as egui's zoom factor: 80–150 %.
pub(crate) const UI_SIZE_MIN: f32 = 0.8;
pub(crate) const UI_SIZE_MAX: f32 = 1.5;
pub(crate) const UI_SIZE_DEFAULT: f32 = 1.0;
/// One Ctrl++ or Ctrl+− press.
pub(crate) const UI_SIZE_STEP: f32 = 0.1;

/// A NaN becomes 100 %; anything else is clamped to the range.
pub(crate) fn sanitize_ui_size(size: f32) -> f32 {
    let size = if size.is_nan() { UI_SIZE_DEFAULT } else { size };
    size.clamp(UI_SIZE_MIN, UI_SIZE_MAX)
}

/// `size` moved by `steps` presses, on the 10 % grid and inside the range.
pub(crate) fn stepped_ui_size(size: f32, steps: i32) -> f32 {
    let size = sanitize_ui_size(size) + steps as f32 * UI_SIZE_STEP;
    sanitize_ui_size((size * 10.0).round() / 10.0)
}

impl RigWerkApp {
    /// Puts the saved theme and size on the context, over what egui kept in
    /// its own memory, and turns egui's keyboard zoom off.  `theme_override`
    /// (a screenshot run's `RIGWERK_THEME`) wins for this run and is not
    /// saved.
    pub(crate) fn apply_appearance(
        &self,
        ctx: &egui::Context,
        theme_override: Option<ThemeChoice>,
    ) {
        ctx.options_mut(|options| {
            options.zoom_with_keyboard = false;
            options.fallback_theme = egui::Theme::Dark;
        });
        ctx.set_theme(drawn_theme(self.theme, theme_override).preference());
        ctx.set_zoom_factor(self.ui_size);
    }

    pub(crate) fn set_theme(&mut self, ctx: &egui::Context, theme: ThemeChoice) {
        self.theme = theme;
        ctx.set_theme(drawn_theme(theme, None).preference());
    }

    pub(crate) fn set_ui_size(&mut self, ctx: &egui::Context, size: f32) {
        self.ui_size = sanitize_ui_size(size);
        ctx.set_zoom_factor(self.ui_size);
    }

    /// Ctrl++ (`steps` 1) and Ctrl+− (`steps` −1).
    pub(crate) fn step_ui_size(&mut self, ctx: &egui::Context, steps: i32) {
        self.set_ui_size(ctx, stepped_ui_size(self.ui_size, steps));
    }
}

#[cfg(test)]
mod tests;
