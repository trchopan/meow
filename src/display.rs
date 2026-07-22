use anyhow::Result;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct DisplayGeometry {
    pub(crate) origin_x: f64,
    pub(crate) origin_y: f64,
    pub(crate) width: f64,
    pub(crate) height: f64,
}

impl DisplayGeometry {
    pub(crate) fn right(&self) -> f64 {
        self.origin_x + self.width
    }

    pub(crate) fn bottom(&self) -> f64 {
        self.origin_y + self.height
    }

    pub(crate) fn center(&self) -> (f64, f64) {
        (
            self.origin_x + self.width / 2.0,
            self.origin_y + self.height / 2.0,
        )
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn main_display_geometry() -> Result<DisplayGeometry> {
    let bounds = core_graphics::display::CGDisplay::main().bounds();
    Ok(DisplayGeometry {
        origin_x: bounds.origin.x,
        origin_y: bounds.origin.y,
        width: bounds.size.width,
        height: bounds.size.height,
    })
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn main_display_geometry() -> Result<DisplayGeometry> {
    let (width, height) = rdev::display_size()
        .map_err(|err| anyhow::anyhow!("failed to determine display size: {err:?}"))?;
    Ok(DisplayGeometry {
        origin_x: 0.0,
        origin_y: 0.0,
        width: width as f64,
        height: height as f64,
    })
}

#[cfg(target_os = "macos")]
pub(crate) fn pointer_location() -> Result<(f64, f64)> {
    use core_graphics::{
        event::CGEvent,
        event_source::{CGEventSource, CGEventSourceStateID},
    };

    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow::anyhow!("failed to create macOS HID event source"))?;
    let location = CGEvent::new(source)
        .map_err(|_| anyhow::anyhow!("failed to read current macOS pointer location"))?
        .location();
    Ok((location.x, location.y))
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn pointer_location() -> Result<(f64, f64)> {
    use enigo::{Enigo, MouseControllable};

    let mut enigo = Enigo::new();
    let (x, y) = enigo.mouse_location();
    Ok((x as f64, y as f64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_preserves_logical_origin_and_center() {
        let display = DisplayGeometry {
            origin_x: 100.0,
            origin_y: 40.0,
            width: 1512.0,
            height: 982.0,
        };

        assert_eq!(display.right(), 1612.0);
        assert_eq!(display.bottom(), 1022.0);
        assert_eq!(display.center(), (856.0, 531.0));
    }
}
