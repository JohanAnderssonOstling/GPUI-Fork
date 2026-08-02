use anyhow::{Context as _, Result};

use crate::PixelRect;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScreenGeometry {
    pub view_width: u32,
    pub view_height: u32,
    pub screen_width: u32,
    pub screen_height: u32,
    pub view_x: u32,
    pub view_y: u32,
    pub current_rotation: u8,
    pub device_name: String,
    pub device_codename: String,
}

impl ScreenGeometry {
    pub fn from_fbink_state(state: &str) -> Result<Self> {
        Ok(Self {
            view_width: number(state, "viewWidth")?,
            view_height: number(state, "viewHeight")?,
            screen_width: number(state, "screenWidth")?,
            screen_height: number(state, "screenHeight")?,
            view_x: number(state, "viewHoriOrigin")?,
            view_y: number(state, "viewVertOrigin")?,
            current_rotation: number::<u8>(state, "currentRota")?,
            device_name: string(state, "deviceName")?,
            device_codename: string(state, "deviceCodename")?,
        })
    }

    pub fn scale_damage(
        &self,
        damage: PixelRect,
        canvas_width: u32,
        canvas_height: u32,
    ) -> PixelRect {
        let x0 = damage.x.saturating_mul(self.view_width) / canvas_width;
        let y0 = damage.y.saturating_mul(self.view_height) / canvas_height;
        let x1 = (damage.x + damage.width)
            .saturating_mul(self.view_width)
            .div_ceil(canvas_width)
            .min(self.view_width);
        let y1 = (damage.y + damage.height)
            .saturating_mul(self.view_height)
            .div_ceil(canvas_height)
            .min(self.view_height);
        PixelRect {
            x: x0,
            y: y0,
            width: x1.saturating_sub(x0).max(1),
            height: y1.saturating_sub(y0).max(1),
        }
    }

    pub fn description(&self) -> String {
        format!(
            "{} ({}) view={}x{}@{},{} screen={}x{} rotation={}",
            self.device_name,
            self.device_codename,
            self.view_width,
            self.view_height,
            self.view_x,
            self.view_y,
            self.screen_width,
            self.screen_height,
            self.current_rotation
        )
    }
}

fn field<'a>(state: &'a str, name: &str) -> Result<&'a str> {
    state
        .split(';')
        .find_map(|part| part.strip_prefix(&format!("{name}=")))
        .ok_or_else(|| anyhow::anyhow!("FBInk state omitted {name}"))
}

fn number<T>(state: &str, name: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    field(state, name)?
        .parse()
        .with_context(|| format!("FBInk returned an invalid {name}"))
}

fn string(state: &str, name: &str) -> Result<String> {
    Ok(field(state, name)?.trim_matches('\'').to_string())
}
