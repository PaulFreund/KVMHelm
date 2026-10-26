//! Vendor-independent DOM HID planning; adapters own delivery and connections.
use super::{ActionContext, Delivery};
use crate::{
    Result,
    contracts::{Action, Button, Layout, Point},
    keyboard::{key_name, text_keys},
};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{collections::BTreeSet, sync::Mutex, time::Duration};

#[async_trait]
pub trait HidTransport: Send + Sync {
    fn key_interval(&self) -> Duration {
        Duration::ZERO
    }
    async fn event(&self, kind: &str, value: Value, context: Option<&ActionContext>) -> Result<()>;
}
#[derive(Default)]
pub struct Hid {
    keys: Mutex<BTreeSet<String>>,
    buttons: Mutex<BTreeSet<String>>,
}
impl Hid {
    pub fn clear(&self) {
        self.keys.lock().unwrap().clear();
        self.buttons.lock().unwrap().clear();
    }
    pub fn has_pressed(&self) -> bool {
        !self.keys.lock().unwrap().is_empty() || !self.buttons.lock().unwrap().is_empty()
    }
    async fn chord(
        &self,
        transport: &dyn HidTransport,
        keys: &[String],
        context: &ActionContext,
    ) -> Result<()> {
        let interval = transport.key_interval();
        let pressed = async {
            for key in keys {
                self.keys.lock().unwrap().insert(key.clone());
                transport
                    .event("key", json!({"key":key,"state":true}), Some(context))
                    .await?;
                if !interval.is_zero() {
                    context.delay(interval).await?;
                }
            }
            Ok(())
        }
        .await;
        let mut released = Ok(());
        for key in keys.iter().rev() {
            match transport
                .event("key", json!({"key":key,"state":false}), None)
                .await
            {
                Ok(()) => {
                    self.keys.lock().unwrap().remove(key);
                }
                Err(error) => released = Err(error),
            }
            // Releases must finish even if cancellation interrupted a hold.
            if !interval.is_zero() {
                tokio::time::sleep(interval).await;
            }
        }
        pressed.and(released)
    }
    async fn button(
        &self,
        transport: &dyn HidTransport,
        button: &str,
        state: bool,
        context: &ActionContext,
    ) -> Result<()> {
        if state {
            self.buttons.lock().unwrap().insert(button.into());
        }
        transport
            .event(
                "mouse_button",
                json!({"button":button,"state":state}),
                state.then_some(context),
            )
            .await?;
        if !state {
            self.buttons.lock().unwrap().remove(button);
        }
        Ok(())
    }
    async fn move_to(
        &self,
        transport: &dyn HidTransport,
        point: &Point,
        context: &ActionContext,
    ) -> Result<()> {
        // Math.round in JavaScript rounds negative ties toward positive infinity.
        let coordinate = |value: i64, dimension: u32| {
            ((value as f64 / f64::from(dimension.saturating_sub(1).max(1)) * 65535.0 - 32768.0)
                + 0.5)
                .floor() as i64
        };
        transport.event("mouse_move",json!({"to":{"x":coordinate(point.x,context.width),"y":coordinate(point.y,context.height)}}),Some(context)).await
    }
    pub async fn execute(
        &self,
        transport: &dyn HidTransport,
        action: &Action,
        context: &ActionContext,
        layout: Layout,
    ) -> Result<Delivery> {
        context.check()?;
        if let Some(point) = action.point() {
            self.move_to(transport, &point, context).await?;
        }
        match action {
            Action::Click { .. } | Action::DoubleClick { .. } => {
                let name = match action {
                    Action::Click {
                        button: Button::Right,
                        ..
                    } => "right",
                    Action::Click {
                        button: Button::Middle,
                        ..
                    } => "middle",
                    _ => "left",
                };
                let count = if matches!(action, Action::DoubleClick { .. }) {
                    2
                } else {
                    1
                };
                for index in 0..count {
                    let pressed = self.button(transport, name, true, context).await;
                    let released = self.button(transport, name, false, context).await;
                    pressed.and(released)?;
                    if index + 1 < count {
                        context.delay(Duration::from_millis(60)).await?;
                    }
                }
            }
            Action::Drag { path } => {
                self.move_to(transport, &path[0], context).await?;
                let drag = async {
                    self.button(transport, "left", true, context).await?;
                    for point in &path[1..] {
                        self.move_to(transport, point, context).await?;
                        context.delay(Duration::from_millis(4)).await?;
                    }
                    Ok(())
                }
                .await;
                let released = self.button(transport, "left", false, context).await;
                drag.and(released)?;
            }
            Action::Scroll {
                scroll_x, scroll_y, ..
            } => {
                let mut x = (*scroll_x as f64 / 100.0 + 0.5).floor() as i64;
                let mut y = -(*scroll_y as f64 / 100.0 + 0.5).floor() as i64;
                while x != 0 || y != 0 {
                    let dx = x.clamp(-127, 127);
                    let dy = y.clamp(-127, 127);
                    transport
                        .event(
                            "mouse_wheel",
                            json!({"delta":{"x":dx,"y":dy}}),
                            Some(context),
                        )
                        .await?;
                    x -= dx;
                    y -= dy;
                }
            }
            Action::Keypress { keys } => {
                self.chord(
                    transport,
                    &keys
                        .iter()
                        .map(|key| key_name(key))
                        .collect::<Result<Vec<_>>>()?,
                    context,
                )
                .await?
            }
            Action::Type { text } => {
                for keys in text_keys(text, layout)? {
                    self.chord(transport, &keys, context).await?;
                }
            }
            _ => (),
        }
        Ok(Delivery::Sent)
    }
    pub async fn release(&self, transport: &dyn HidTransport) -> Result<()> {
        let keys = self
            .keys
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        let buttons = self
            .buttons
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        let mut released = Ok(());
        for key in keys {
            match transport
                .event("key", json!({"key":key,"state":false}), None)
                .await
            {
                Ok(()) => {
                    self.keys.lock().unwrap().remove(&key);
                }
                Err(error) => released = Err(error),
            }
        }
        for button in buttons {
            match transport
                .event("mouse_button", json!({"button":button,"state":false}), None)
                .await
            {
                Ok(()) => {
                    self.buttons.lock().unwrap().remove(&button);
                }
                Err(error) => released = Err(error),
            }
        }
        let control = transport
            .event("key", json!({"key":"ControlLeft","state":false}), None)
            .await;
        released.and(control)
    }
}
