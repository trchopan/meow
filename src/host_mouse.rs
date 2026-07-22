use anyhow::Result;

#[cfg(target_os = "macos")]
mod imp {
    use anyhow::{Result, anyhow};
    use core_graphics::{display::CGDisplay, geometry::CGPoint};

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

pub(crate) fn center_pointer() -> Result<(f64, f64)> {
    let (width, height) = rdev::display_size().map_err(|err| {
        anyhow::anyhow!("failed to determine display size for pointer centering: {err:?}")
    })?;
    if width == 0 || height == 0 {
        return Err(anyhow::anyhow!("display size is zero"));
    }

    let position = (width as f64 / 2.0, height as f64 / 2.0);
    warp_pointer(position.0, position.1)?;
    Ok(position)
}

pub(crate) fn set_pointer_visible(visible: bool) -> Result<()> {
    imp::set_pointer_visible(visible)
}
