use std::collections::VecDeque;

use futures::StreamExt;
use log::info;
use tokio::time::{Duration, Instant};
use zbus::{Connection, proxy};

use crate::state::KeyboardStateManager;

const AMBIENT_STABILITY_WINDOW: Duration = Duration::from_secs(20);
const AMBIENT_STABILITY_DRIFT_RATIO: f64 = 0.10;
const AMBIENT_MIN_STABLE_DRIFT_LUX: f64 = 1.0;

#[proxy(
    interface = "net.hadess.SensorProxy",
    default_service = "net.hadess.SensorProxy",
    default_path = "/net/hadess/SensorProxy"
)]
trait SensorProxy {
    fn claim_light(&self) -> zbus::Result<()>;
    fn release_light(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn has_ambient_light(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn light_level(&self) -> zbus::Result<f64>;
}

struct AmbientLightSmoother {
    stable_lux: f64,
    samples: VecDeque<(Instant, f64)>,
}

impl AmbientLightSmoother {
    fn new(initial_lux: f64, now: Instant) -> Self {
        let mut samples = VecDeque::new();
        samples.push_back((now, initial_lux));
        Self {
            stable_lux: initial_lux,
            samples,
        }
    }

    fn required_drift_lux(&self) -> f64 {
        (self.stable_lux.abs() * AMBIENT_STABILITY_DRIFT_RATIO).max(AMBIENT_MIN_STABLE_DRIFT_LUX)
    }

    fn record_sample(&mut self, lux: f64, now: Instant) -> Option<f64> {
        self.samples.push_back((now, lux));
        let window_start = now - AMBIENT_STABILITY_WINDOW;
        while self
            .samples
            .front()
            .is_some_and(|(timestamp, _)| *timestamp < window_start)
        {
            self.samples.pop_front();
        }

        let oldest = self.samples.front()?.0;
        if now.duration_since(oldest) < AMBIENT_STABILITY_WINDOW {
            return None;
        }

        let average =
            self.samples.iter().map(|(_, sample)| *sample).sum::<f64>() / self.samples.len() as f64;
        if (average - self.stable_lux).abs() < self.required_drift_lux() {
            return None;
        }

        self.stable_lux = average;
        self.samples.clear();
        self.samples.push_back((now, average));
        Some(average)
    }
}

pub async fn run(state_manager: KeyboardStateManager) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let conn = Connection::system().await?;
    let sensor = SensorProxyProxy::new(&conn).await?;

    if !sensor.has_ambient_light().await? {
        state_manager.set_ambient_light_sensor_available(false).await;
        info!("Ambient light sensor not available");
        std::future::pending::<()>().await;
        return Ok(());
    }

    sensor.claim_light().await?;
    state_manager.set_ambient_light_sensor_available(true).await;

    let initial = sensor.light_level().await?;
    state_manager.set_ambient_light_lux(Some(initial)).await;
    info!("Ambient light claimed; initial light level: {initial:.3} lux");

    let mut smoother = AmbientLightSmoother::new(initial, Instant::now());
    let mut light_level_changes = sensor.receive_light_level_changed().await;

    loop {
        if light_level_changes.next().await.is_none() {
            return Err("ambient light change stream ended".into());
        }

        let level = sensor.light_level().await?;
        if let Some(stable_level) = smoother.record_sample(level, Instant::now()) {
            state_manager.set_ambient_light_lux(Some(stable_level)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoother_waits_for_full_window() {
        let start = Instant::now();
        let mut smoother = AmbientLightSmoother::new(100.0, start);

        assert_eq!(smoother.record_sample(85.0, start + Duration::from_secs(19)), None);
    }

    #[test]
    fn smoother_emits_after_sustained_large_drift() {
        let start = Instant::now();
        let mut smoother = AmbientLightSmoother::new(100.0, start);

        let stable = smoother.record_sample(75.0, start + Duration::from_secs(20));

        assert_eq!(stable, Some(87.5));
    }

    #[test]
    fn smoother_ignores_small_drift() {
        let start = Instant::now();
        let mut smoother = AmbientLightSmoother::new(100.0, start);

        let stable = smoother.record_sample(96.0, start + Duration::from_secs(20));

        assert_eq!(stable, None);
    }
}
