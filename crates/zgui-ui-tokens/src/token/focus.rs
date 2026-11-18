//! How keyboard focus is framed around a control.
//!
//! By default, interactive controls draw a three-pixel outer halo in half-strength
//! [`ColorTokens::ring`](crate::ColorTokens) around their border box. Host design systems that
//! express focus with a crisp border or inset contour instead (for example to prevent outer halos
//! from clipping at scrollport edges) set `--zui-focus-ring-shadow: none;` — or call
//! [`FocusTokens::disable_ring_shadows`] — at the token layer without specificity overrides.

use crate::token::group::group;

group! {
    /// How keyboard focus is framed around a control.
    FocusTokens, prefix = "focus", {
        /// The outer shadow halo drawn around a focused control.
        ring_shadow => "ring-shadow",
            light = "0 0 0 3px color-mix(in oklab, var(--zui-color-ring) 50%, transparent)",
            dark = "0 0 0 3px color-mix(in oklab, var(--zui-color-ring) 50%, transparent)";
        /// The outer shadow halo drawn around an invalid or destructive focused control.
        ring_invalid_shadow => "ring-invalid-shadow",
            light = "0 0 0 3px var(--zui-color-control-ring-invalid)",
            dark = "0 0 0 3px var(--zui-color-control-ring-invalid)";
        /// The outer shadow halo drawn around a focused control inside a sidebar.
        sidebar_ring_shadow => "sidebar-ring-shadow",
            light = "0 0 0 3px color-mix(in oklab, var(--zui-color-sidebar-ring) 50%, transparent)",
            dark = "0 0 0 3px color-mix(in oklab, var(--zui-color-sidebar-ring) 50%, transparent)";
        /// The inset shadow halo drawn inside a clipped strip such as a scroll bar.
        ring_inset_shadow => "ring-inset-shadow",
            light = "inset 0 0 0 3px color-mix(in oklab, var(--zui-color-ring) 50%, transparent)",
            dark = "inset 0 0 0 3px color-mix(in oklab, var(--zui-color-ring) 50%, transparent)";
        /// The outline drawn on borderless dismiss and action controls when focused.
        ring_outline => "ring-outline",
            light = "2px solid var(--zui-color-ring)",
            dark = "2px solid var(--zui-color-ring)";
        /// The outline offset paired with [`FocusTokens::ring_outline`].
        ring_offset => "ring-offset",
            light = "2px",
            dark = "2px";
    }
}

impl FocusTokens {
    /// Turns every outer and inset focus-ring shadow off (`"none"`), leaving borders and outlines
    /// to express focus.
    pub fn disable_ring_shadows(&mut self) {
        self.ring_shadow = String::from("none");
        self.ring_invalid_shadow = String::from("none");
        self.sidebar_ring_shadow = String::from("none");
        self.ring_inset_shadow = String::from("none");
    }
}

#[cfg(test)]
mod tests {
    use super::FocusTokens;

    #[test]
    fn default_focus_tokens_preserve_the_three_pixel_ring_halo() {
        let focus = FocusTokens::light();
        assert_eq!(focus, FocusTokens::dark());
        assert_eq!(
            focus.ring_shadow,
            "0 0 0 3px color-mix(in oklab, var(--zui-color-ring) 50%, transparent)"
        );
        assert_eq!(
            focus.ring_invalid_shadow,
            "0 0 0 3px var(--zui-color-control-ring-invalid)"
        );
        assert!(FocusTokens::PROPERTIES.contains(&"--zui-focus-ring-shadow"));
    }

    #[test]
    fn disabling_ring_shadows_clears_all_shadow_halos_and_keeps_outline_tokens() {
        let mut focus = FocusTokens::light();
        focus.disable_ring_shadows();
        assert_eq!(focus.ring_shadow, "none");
        assert_eq!(focus.ring_invalid_shadow, "none");
        assert_eq!(focus.sidebar_ring_shadow, "none");
        assert_eq!(focus.ring_inset_shadow, "none");
        assert_eq!(focus.ring_outline, "2px solid var(--zui-color-ring)");
    }
}
