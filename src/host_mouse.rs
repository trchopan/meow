use anyhow::Result;

#[cfg(target_os = "macos")]
mod imp {
    use anyhow::{Result, anyhow};
    use core_graphics::{
        display::CGDisplay,
        event::CGEvent,
        event_source::{CGEventSource, CGEventSourceStateID},
        geometry::CGPoint,
    };

    pub(crate) fn set_pointer_dissociation(enabled: bool) -> Result<()> {
        CGDisplay::associate_mouse_and_mouse_cursor_position(!enabled).map_err(|err| {
            anyhow!(
                "CGAssociateMouseAndMouseCursorPosition({}) failed with code {}",
                if enabled { 0 } else { 1 },
                err
            )
        })
    }

    pub(crate) fn warp_pointer(x: f64, y: f64) -> Result<()> {
        CGDisplay::warp_mouse_cursor_position(CGPoint::new(x, y))
            .map_err(|err| anyhow!("CGWarpMouseCursorPosition failed with code {}", err))
    }

    pub(crate) fn current_pointer_position() -> Result<(f64, f64)> {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| anyhow!("failed to create CGEventSource"))?;
        let event = CGEvent::new(source).map_err(|_| anyhow!("failed to create CGEvent"))?;
        let location = event.location();
        Ok((location.x, location.y))
    }

    pub(crate) fn set_pointer_visible(visible: bool) -> Result<()> {
        let display = CGDisplay::main();
        let result = if visible {
            display.show_cursor()
        } else {
            display.hide_cursor()
        };
        result.map_err(|err| {
            anyhow!(
                "{} failed with code {}",
                if visible {
                    "CGDisplayShowCursor"
                } else {
                    "CGDisplayHideCursor"
                },
                err
            )
        })
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use anyhow::Result;

    pub(crate) fn set_pointer_dissociation(_enabled: bool) -> Result<()> {
        Ok(())
    }

    pub(crate) fn warp_pointer(_x: f64, _y: f64) -> Result<()> {
        Ok(())
    }

    pub(crate) fn current_pointer_position() -> Result<(f64, f64)> {
        Ok((0.0, 0.0))
    }

    pub(crate) fn set_pointer_visible(_visible: bool) -> Result<()> {
        Ok(())
    }
}

pub(crate) fn set_pointer_dissociation(enabled: bool) -> Result<()> {
    imp::set_pointer_dissociation(enabled)
}

pub(crate) fn warp_pointer(x: f64, y: f64) -> Result<()> {
    imp::warp_pointer(x, y)
}

pub(crate) fn current_pointer_position() -> Result<(f64, f64)> {
    imp::current_pointer_position()
}

pub(crate) fn center_pointer() -> Result<(f64, f64)> {
    let position = crate::display::main_display_geometry()?.center();
    warp_pointer(position.0, position.1)?;
    Ok(position)
}

pub(crate) fn set_pointer_visible(visible: bool) -> Result<()> {
    imp::set_pointer_visible(visible)
}
