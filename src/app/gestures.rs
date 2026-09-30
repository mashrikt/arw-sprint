//! Map input deltas to inexpensive viewport transforms. No photo work here.
use winit::event::MouseScrollDelta;

#[derive(Debug, PartialEq)]
pub(super) enum Scroll {
    Pan(f64, f64),
    Zoom(f64),
}

fn scale(value: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        1.0
    }
}

pub(super) fn scroll(delta: MouseScrollDelta, shift: bool, display_scale: f64) -> Scroll {
    match delta {
        MouseScrollDelta::PixelDelta(p) if !shift => Scroll::Pan(p.x, p.y),
        MouseScrollDelta::PixelDelta(p) => {
            // AppKit may swap the scroll axes while Shift is held.
            let movement = if p.y.abs() >= p.x.abs() { p.y } else { p.x };
            Scroll::Zoom(zoom_factor(movement / scale(display_scale) * 0.005))
        }
        // A traditional mouse wheel keeps its familiar zoom behavior.
        MouseScrollDelta::LineDelta(x, y) => {
            let movement = if y.abs() >= x.abs() { y } else { x };
            Scroll::Zoom(zoom_factor(f64::from(movement) * 0.15))
        }
    }
}

pub(super) fn drag_zoom(dy: f64, display_scale: f64) -> f64 {
    // Shift-pointer upwards zooms in around the gesture anchor; downwards zooms out.
    zoom_factor(-dy / scale(display_scale) * 0.008)
}

fn zoom_factor(exponent: f64) -> f64 {
    if exponent.is_finite() {
        exponent.clamp(-2.0, 2.0).exp()
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::dpi::PhysicalPosition;

    #[test]
    fn trackpad_pans_both_axes_one_physical_pixel_at_a_time() {
        assert_eq!(
            scroll(
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(12.0, -28.0)),
                false,
                2.0
            ),
            Scroll::Pan(12.0, -28.0)
        );
    }

    #[test]
    fn shifted_scroll_zooms_with_retina_and_swapped_axes() {
        let normal = scroll(
            MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 20.0)),
            true,
            1.0,
        );
        assert_eq!(
            normal,
            scroll(
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 40.0)),
                true,
                2.0
            )
        );
        assert_eq!(
            normal,
            scroll(
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(20.0, 0.0)),
                true,
                1.0
            )
        );
        assert_eq!(normal, Scroll::Zoom(0.1f64.exp()));
    }

    #[test]
    fn drag_zoom_is_reversible_bounded_and_scale_independent() {
        assert!(drag_zoom(-50.0, 1.0) > 1.0);
        assert_eq!(drag_zoom(-50.0, 1.0), drag_zoom(-100.0, 2.0));
        assert!((drag_zoom(-50.0, 1.0) * drag_zoom(50.0, 1.0) - 1.0).abs() < 1e-12);
        assert_eq!(drag_zoom(f64::NAN, 1.0), 1.0);
        assert_eq!(drag_zoom(f64::INFINITY, 1.0), 1.0);
        assert_eq!(drag_zoom(-f64::MAX, 1.0), 2.0f64.exp());
    }
}
