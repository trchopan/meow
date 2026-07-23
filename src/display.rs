use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

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

    pub(crate) fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.origin_x && x < self.right() && y >= self.origin_y && y < self.bottom()
    }

    pub(crate) fn clamp_pointer_move(
        &self,
        x: f64,
        y: f64,
        dx: i32,
        dy: i32,
    ) -> Option<(f64, f64, i32, i32)> {
        if self.width <= 0.0 || self.height <= 0.0 || !x.is_finite() || !y.is_finite() {
            return None;
        }

        let min_x = self.origin_x;
        let min_y = self.origin_y;
        let max_x = self.right() - 1.0;
        let max_y = self.bottom() - 1.0;
        let target_x = (x + f64::from(dx)).clamp(min_x, max_x);
        let target_y = (y + f64::from(dy)).clamp(min_y, max_y);
        let actual_dx = (target_x - x).round() as i32;
        let actual_dy = (target_y - y).round() as i32;
        Some((target_x, target_y, actual_dx, actual_dy))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DisplayLayout {
    pub(crate) displays: Vec<DisplayGeometry>,
    pub(crate) main: DisplayGeometry,
}

impl DisplayLayout {
    pub(crate) fn main(&self) -> DisplayGeometry {
        self.main
    }

    pub(crate) fn display_at(&self, x: f64, y: f64) -> Option<DisplayGeometry> {
        self.displays
            .iter()
            .copied()
            .find(|display| display.contains(x, y))
    }

    fn nearest_display(&self, x: f64, y: f64) -> Option<DisplayGeometry> {
        self.displays.iter().copied().min_by(|left, right| {
            distance_to_display(left, x, y).total_cmp(&distance_to_display(right, x, y))
        })
    }

    pub(crate) fn clamp_absolute_point(
        &self,
        x: f64,
        y: f64,
        fallback_x: f64,
        fallback_y: f64,
    ) -> Option<(f64, f64)> {
        if !x.is_finite() || !y.is_finite() || !fallback_x.is_finite() || !fallback_y.is_finite() {
            return None;
        }
        let display = self
            .display_at(x, y)
            .or_else(|| self.display_at(fallback_x, fallback_y))
            .or_else(|| self.nearest_display(x, y))?;
        Some((
            x.clamp(display.origin_x, display.right() - 1.0),
            y.clamp(display.origin_y, display.bottom() - 1.0),
        ))
    }

    pub(crate) fn clamp_pointer_move(
        &self,
        x: f64,
        y: f64,
        dx: i32,
        dy: i32,
    ) -> Option<(f64, f64, i32, i32)> {
        let display = self
            .display_at(x, y)
            .or_else(|| self.nearest_display(x, y))?;
        let requested_x = x + f64::from(dx);
        let requested_y = y + f64::from(dy);
        if self.display_at(requested_x, requested_y).is_some() {
            return Some((requested_x, requested_y, dx, dy));
        }
        display.clamp_pointer_move(x, y, dx, dy)
    }
}

fn distance_to_display(display: &DisplayGeometry, x: f64, y: f64) -> f64 {
    let dx = if x < display.origin_x {
        display.origin_x - x
    } else if x >= display.right() {
        x - display.right()
    } else {
        0.0
    };
    let dy = if y < display.origin_y {
        display.origin_y - y
    } else if y >= display.bottom() {
        y - display.bottom()
    } else {
        0.0
    };
    dx.mul_add(dx, dy * dy)
}

const DISPLAY_LAYOUT_CACHE_TTL: Duration = Duration::from_millis(100);

struct DisplayLayoutCache {
    layout: Option<DisplayLayout>,
    refreshed_at: Instant,
}

static DISPLAY_LAYOUT_CACHE: OnceLock<Mutex<DisplayLayoutCache>> = OnceLock::new();

pub(crate) fn display_layout() -> Result<DisplayLayout> {
    let cache = DISPLAY_LAYOUT_CACHE.get_or_init(|| {
        Mutex::new(DisplayLayoutCache {
            layout: None,
            refreshed_at: Instant::now() - DISPLAY_LAYOUT_CACHE_TTL,
        })
    });
    let mut cache = cache
        .lock()
        .map_err(|_| anyhow::anyhow!("display layout cache mutex poisoned"))?;
    if cache.layout.is_none() || cache.refreshed_at.elapsed() >= DISPLAY_LAYOUT_CACHE_TTL {
        cache.layout = Some(query_display_layout()?);
        cache.refreshed_at = Instant::now();
    }
    cache
        .layout
        .clone()
        .ok_or_else(|| anyhow::anyhow!("display layout is empty"))
}

#[cfg(target_os = "macos")]
pub(crate) fn main_display_geometry() -> Result<DisplayGeometry> {
    Ok(display_layout()?.main())
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
fn query_display_layout() -> Result<DisplayLayout> {
    use core_graphics::display::CGDisplay;

    let ids = CGDisplay::active_displays()
        .map_err(|err| anyhow::anyhow!("failed to enumerate macOS displays: {err}"))?;
    let main_id = CGDisplay::main().id;
    let displays = ids
        .into_iter()
        .filter_map(|id| {
            let bounds = CGDisplay::new(id).bounds();
            let display = DisplayGeometry {
                origin_x: bounds.origin.x,
                origin_y: bounds.origin.y,
                width: bounds.size.width,
                height: bounds.size.height,
            };
            (display.width > 0.0 && display.height > 0.0).then_some((id, display))
        })
        .collect::<Vec<_>>();
    let main = displays
        .iter()
        .find(|(id, _)| *id == main_id)
        .map(|(_, display)| *display)
        .ok_or_else(|| anyhow::anyhow!("macOS main display is not active"))?;
    if displays.is_empty() {
        return Err(anyhow::anyhow!("macOS display layout is empty"));
    }
    Ok(DisplayLayout {
        displays: displays.into_iter().map(|(_, display)| display).collect(),
        main,
    })
}

#[cfg(not(target_os = "macos"))]
fn query_display_layout() -> Result<DisplayLayout> {
    let (width, height) = rdev::display_size()
        .map_err(|err| anyhow::anyhow!("failed to determine display size: {err:?}"))?;
    let main = DisplayGeometry {
        origin_x: 0.0,
        origin_y: 0.0,
        width: width as f64,
        height: height as f64,
    };
    Ok(DisplayLayout {
        displays: vec![main],
        main,
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

    #[test]
    fn clamp_pointer_move_keeps_target_inside_logical_bounds() {
        let display = DisplayGeometry {
            origin_x: 100.0,
            origin_y: 40.0,
            width: 1512.0,
            height: 982.0,
        };

        assert_eq!(
            display.clamp_pointer_move(1611.0, 1021.0, 12, 12),
            Some((1611.0, 1021.0, 0, 0))
        );
        assert_eq!(
            display.clamp_pointer_move(1608.0, 500.0, 12, 0),
            Some((1611.0, 500.0, 3, 0))
        );
        assert_eq!(
            display.clamp_pointer_move(100.0, 40.0, -12, -12),
            Some((100.0, 40.0, 0, 0))
        );
        assert_eq!(
            display.clamp_pointer_move(856.0, 531.0, 12, -8),
            Some((868.0, 523.0, 12, -8))
        );
    }

    #[test]
    fn layout_allows_relative_motion_into_adjacent_display() {
        let layout = DisplayLayout {
            displays: vec![
                DisplayGeometry {
                    origin_x: 0.0,
                    origin_y: 0.0,
                    width: 100.0,
                    height: 100.0,
                },
                DisplayGeometry {
                    origin_x: 100.0,
                    origin_y: 0.0,
                    width: 200.0,
                    height: 100.0,
                },
            ],
            main: DisplayGeometry {
                origin_x: 0.0,
                origin_y: 0.0,
                width: 100.0,
                height: 100.0,
            },
        };

        assert_eq!(
            layout.clamp_pointer_move(90.0, 50.0, 20, 0),
            Some((110.0, 50.0, 20, 0))
        );
    }

    #[test]
    fn absolute_point_in_gap_falls_back_to_current_display() {
        let left = DisplayGeometry {
            origin_x: 0.0,
            origin_y: 0.0,
            width: 100.0,
            height: 100.0,
        };
        let right = DisplayGeometry {
            origin_x: 200.0,
            origin_y: 0.0,
            width: 100.0,
            height: 100.0,
        };
        let layout = DisplayLayout {
            displays: vec![left, right],
            main: left,
        };

        assert_eq!(
            layout.clamp_absolute_point(150.0, 50.0, 20.0, 50.0),
            Some((99.0, 50.0))
        );
    }
}
