use crate::ble::TargetSpeed;
use crate::hass_mqtt::base::{Device, EntityConfig, Origin};
use crate::hass_mqtt::instance::{publish_entity_config, EntityInstance};
use crate::hass_mqtt::work_mode::ParsedWorkMode;
use crate::platform_api::{DeviceParameters, DeviceType, IntegerRange};
use crate::service::device::Device as ServiceDevice;
use crate::service::hass::{availability_topic, topic_safe_id, HassClient, IdParameter};
use crate::service::state::StateHandle;
use anyhow::anyhow;
use async_trait::async_trait;
use mosquitto_rs::router::{Params, Payload, State};
use serde::Serialize;
use serde_json::json;

pub const DEVICE_CLASS_FAN: &str = "fan";

/// <https://www.home-assistant.io/integrations/fan.mqtt>
#[derive(Serialize, Clone, Debug)]
pub struct FanConfig {
    #[serde(flatten)]
    pub base: EntityConfig,

    pub command_topic: String,
    /// HASS will publish here to change the fan oscillation state
    pub oscillation_command_topic: String,
    /// HASS will subscribe here to receive the oscillation state
    pub oscillation_state_topic: String,

    /// HASS will publish here to change the current mode
    pub preset_mode_command_topic: String,
    /// we will publish the current mode here
    pub preset_mode_state_topic: String,

    /// HASS will publsh here to change the current speed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percentage_command_topic: Option<u8>
    /// we will publsh here the current speed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percentage_state_topic: Option<u8>
    /// we will publish the max speed here
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed_range_max: Option<u8>,
    /// we will publish the min speed here
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed_range_min: Option<u8>

    pub optimistic: bool,

    /// The list of supported modes
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub preset_modes: Vec<String>,

    pub state_topic: String,
}

#[derive(Clone)]
pub struct Fan {
    fan: FanConfig,
    state: StateHandle,
    device_id: String,
}

impl Fan {
    pub async fn new(device: &ServiceDevice, state: &StateHandle) -> anyhow::Result<Self> {
        let _quirk = device.resolve_quirk();
        let use_iot = device.iot_api_supported() && state.get_iot_client().await.is_some();
        let optimistic = !use_iot;

        let device_class = Some("fan")

        // command_topic controls the power state; just route it to
        // the general power switch handler
        let command_topic = format!(
            "gv2mqtt/switch/{id}/command/powerSwitch",
            id = topic_safe_id(device)
        );

        let oscillation_command_topic = format!(
            "gv2mqtt/fan/{id}/set-oscillation",
            id = topic_safe_id(device)
        );
        let oscillation_state_topic = format!(
            "gv2mqtt/fan/{id}/notify-oscillation",
            id = topic_safe_id(device)
        );
        let state_topic = format!("gv2mqtt/fan/{id}/state", id = topic_safe_id(device));

        let mode_command_topic = format!(
            "gv2mqtt/fan/{id}/set-mode",
            id = topic_safe_id(device)
        );
        let mode_state_topic = format!(
            "gv2mqtt/fan/{id}/notify-mode",
            id = topic_safe_id(device)
        );

        let percentage_command_topic = format!(
            "gv2mqtt/fan/{id}/set-speed",
            id = topic_safe_id(device)
        );
        let percentage_state_topic = format!(
            "gv2mqtt/fan/{id}/notify-speed",
            id = topic_safe_id(device)
        );

        let unique_id = format!("gv2mqtt-{id}-fan", id = topic_safe_id(device),);

        let mut min_speed = None;
        let mut max_speed = None;

        let work_mode = ParsedWorkMode::with_device(device).ok();
        let modes = work_mode
            .as_ref()
            .map(|wm| wm.get_mode_names())
            .unwrap_or(vec![]);

        if let Some(info) = &device.http_device_info {
            if let Some(cap) = info.capability_by_instance("fan") {
                match &cap.parameters {
                    Some(DeviceParameters::Integer {
                        range: IntegerRange { min, max, .. },
                        unit,
                    }) => {
                        if unit.as_deref() == Some("unit.percent") {
                            min_speed.replace(*min as u8);
                            max_speed.replace(*max as u8);
                        }
                    }
                    _ => {}
                }
            }
        }

        Ok(Self {
            fan: FanConfig {
                base: EntityConfig {
                    availability_topic: availability_topic(),
                    name: if matches!(
                        device.device_type(),
                        DeviceType::Fan
                    ) {
                        None
                    } else {
                        Some("Fan".to_string())
                    },
                    device_class,
                    origin: Origin::default(),
                    device: Device::for_device(device),
                    unique_id,
                    entity_category: None,
                    icon: None,
                },
                command_topic,
                oscillation_command_topic,
                oscillation_state_topic,

                speed_range_min,
                speed_range_max,

                percentage_command_topic,
                percentage_state_topic

                preset_mode_command_topic,
                preset_mode_state_topic,
                preset_modes,
                state_topic,
                optimistic,
            },
            device_id: device.id.to_string(),
            state: state.clone(),
        })
    }
}

#[async_trait]
impl EntityInstance for Fan {
    async fn publish_config(&self, state: &StateHandle, client: &HassClient) -> anyhow::Result<()> {
        publish_entity_config(
            "fan",
            state,
            client,
            &self.fan.base,
            &self.fan,
        )
        .await
    }

    async fn notify_state(&self, client: &HassClient) -> anyhow::Result<()> {
        let device = self
            .state
            .device_by_id(&self.device_id)
            .await
            .expect("device to exist");

        match device.device_state() {
            Some(device_state) => {
                let is_on = device_state.on;
                client
                    .publish(
                        &self.fan.state_topic,
                        if is_on { "ON" } else { "OFF" },
                    )
                    .await?;
            }
            None => {
                client.publish(&self.fan.state_topic, "OFF").await?;
            }
        }

        if let Some(speed) = device.percentage_state_topic {
            client
                .publish(
                    &self.fan.percentage_state_topic,
                    speed.to_string(),
                )
                .await?;
        } else {
            // We need an initial value otherwise hass will not enable
            // the target speed control in its UI.
            // Because we are setting this in the device state,
            // this latches so we only do this once.
            let guessed_value = self.fan.speed_range_min.unwrap_or(0);
            self.state
                .device_mut(&device.sku, &device.id)
                .await
                .set_target_speed(guessed_value);
            client
                .publish(
                    &self.fan.percentage_state_topic,
                    guessed_value.to_string(),
                )
                .await?;
        }

        if let Some(mode_value) = device.fan_work_mode {
            if let Ok(work_mode) = ParsedWorkMode::with_device(&device) {
                let mode_value_json = json!(mode_value);
                if let Some(mode) = work_mode.mode_for_value(&mode_value_json) {
                    client
                        .publish(&self.fan.preset_mode_state_topic, mode.name.to_string())
                        .await?;
                }
            }
        } else {
            let work_modes = ParsedWorkMode::with_device(&device)?;

            if let Some(cap) = device.get_state_capability_by_instance("workMode") {
                if let Some(mode_num) = cap.state.pointer("/value/workMode") {
                    if let Some(mode) = work_modes.mode_for_value(mode_num) {
                        return client
                            .publish(&self.fan.preset_mode_state_topic, mode.name.to_string())
                            .await;
                    }
                }
            }
        }
        Ok(())
    }
}

pub async fn mqtt_fan_set_work_mode(
    Payload(mode): Payload<String>,
    Params(IdParameter { id }): Params<IdParameter>,
    State(state): State<StateHandle>,
) -> anyhow::Result<()> {
    log::info!("mqtt_fan_set_mode: {id}: {mode}");
    let device = state.resolve_device_for_control(&id).await?;

    let work_modes = ParsedWorkMode::with_device(&device)?;
    let work_mode = work_modes
        .mode_by_name(&mode)
        .ok_or_else(|| anyhow!("mode {mode} not found"))?;
    let mode_num = work_mode
        .value
        .as_i64()
        .ok_or_else(|| anyhow::anyhow!("expected workMode to be a number"))?;

    let value = work_mode.default_value();

    state
        .fan_set_parameter(&device, mode_num, value)
        .await?;

    Ok(())
}

pub async fn mqtt_fan_set_speed(
    Payload(percent): Payload<i64>,
    Params(IdParameter { id }): Params<IdParameter>,
    State(state): State<StateHandle>,
) -> anyhow::Result<()> {
    log::info!("mqtt_fan_set_speed: {id}: {percent}");

    let device = state.resolve_device_for_control(&id).await?;

    let use_iot = device.pollable_via_iot() && state.get_iot_client().await.is_some();

    if !use_iot {
        if let Some(info) = &device.http_device_info {
            if let Some(cap) = info.capability_by_instance("fan") {
                state.device_control(&device, cap, percent).await?;

                // We're running in optimistic mode; stash
                // the last set value so that we can report it
                // to hass
                state
                    .device_mut(&device.sku, &device.id)
                    .await
                    .set_target_speed(percent as u8);

                // For the H7160 at least, setting the fan
                // will put the device into auto mode and turn
                // it on, however, we don't know that the device
                // is actually turned on.
                //
                // This is handled by the device_was_controlled
                // stuff; it will cause us to poll the device
                // after a short delay, and that should fix up
                // the reported device state.
                return Ok(());
            }
        }
    }

    let work_modes = ParsedWorkMode::with_device(&device)?;
    let work_mode = work_modes
        .mode_by_name("Auto")
        .ok_or_else(|| anyhow!("mode Auto not found"))?;
    let mode_num = work_mode
        .value
        .as_i64()
        .ok_or_else(|| anyhow::anyhow!("expected workMode to be a number"))?;

    let value = TargetSpeed::from_percent(percent as u8);

    state
        .fan_set_parameter(&device, mode_num, value.into_inner().into())
        .await?;

    Ok(())
}

async fn mqtt_fan_set_oscillation(
    Payload(oscillate): Payload<bool>,
    Params(IdParameter { id }): Params<IdParameter>,
    State(state): State<StateHandle>,
) -> Result<Response, Response> {
    log::info!("mqtt_fan_set_oscillation: {id}: {mode}");
    let device = state.resolve_device_for_control(&id).await?;

    state
        .fan_set_parameter(&device, oscillate, value)
        .await?;

    Ok(())
}